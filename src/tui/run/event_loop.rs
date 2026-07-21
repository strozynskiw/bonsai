#[cfg(test)]
use super::ctrl_c::CTRL_C_EXIT_WINDOW;
use super::ctrl_c::{
    CtrlCAction, CtrlCExitPrompt, CtrlCExitPromptKind, clear_ctrl_c_prompt,
    clear_expired_ctrl_c_prompt, clears_ctrl_c_prompt, next_ctrl_c_action, set_ctrl_c_prompt,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use background::*;
pub(in crate::tui) use background::{
    open_background_task_list, start_delete_selected_background_task,
};
use scroll::*;

const BACKGROUND_AGENT_WAKE_DELAY: Duration = Duration::from_millis(350);
const PEER_WAIT_RECHECK_DELAY: Duration = Duration::from_secs(10 * 60);
const BACKGROUND_TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PERSISTENCE_FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const FINAL_PERSISTENCE_TIMEOUT: Duration = Duration::from_millis(1000);
const EVENT_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const ACTIVE_REDRAW_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_secs(1);
const TERMINAL_FAST_EMPTY_POLL: Duration = Duration::from_millis(5);
const TERMINAL_FAST_EMPTY_POLL_LIMIT: u8 = 16;
/// Consecutive full-length empty polls that *burned CPU instead of sleeping*
/// before the terminal is declared dead. 10 polls ≈ 0.5s of spin — long enough
/// that a transient scheduler hiccup can't trip it, short enough that a dead
/// tty can't peg a core for more than an eyeblink.
const TERMINAL_BUSY_EMPTY_POLL_LIMIT: u8 = 10;
/// How long after the first SIGTERM/SIGHUP a repeat signal is still treated as
/// a duplicate of the same event rather than an escalation. Past this window a
/// repeat hard-exits the process: the graceful path was asked and had ample
/// time (the shutdown phases cap out around 4s combined), so it is wedged.
const SIGNAL_ESCALATION_GRACE: Duration = Duration::from_secs(5);
/// Pre-poll frame work that starves input long enough for a user to feel the
/// app hang. 10 poll periods: genuine hitches (GC, one slow flush) stay under
/// it; anything over it deserves a named warning in the log.
const SLOW_FRAME_WARN_THRESHOLD: Duration = Duration::from_millis(500);
/// How often the branch watcher re-reads `git HEAD`. The branch only changes
/// when the user runs `git checkout`/`switch` (typically from another terminal),
/// so a couple of seconds keeps the header honest without polling git hard.
const BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// How long the very first turn-start waits for a still-running repo-map build.
/// The map lives in the byte-stable cacheable prefix, so applying it *before*
/// the first request avoids the one mid-session prefix shift that costs the
/// provider prompt cache; past this bound the turn proceeds without the map
/// (one shift later, as before).
const REPO_MAP_FIRST_TURN_WAIT: Duration = Duration::from_millis(750);

/// Wake authority is attached to durable message identities, never to sticky
/// aggregate booleans. The current durable agent inbox is re-read at decision
/// time, which removes authority as soon as the exact message is acknowledged.
#[derive(Debug)]
struct PendingPeerWake {
    deadline: Instant,
    messages: BTreeMap<i64, crate::storage::PeerMessage>,
}

#[derive(Debug, Clone, Copy)]
struct PendingPeerWaitRecheck {
    wait: crate::agent::PeerWait,
    deadline: Instant,
}

impl PendingPeerWaitRecheck {
    fn new(wait: crate::agent::PeerWait, now: Instant) -> Self {
        Self {
            wait,
            deadline: now + PEER_WAIT_RECHECK_DELAY,
        }
    }
}

impl PendingPeerWake {
    fn merge(&mut self, messages: impl IntoIterator<Item = crate::storage::PeerMessage>) {
        let mut merged = false;
        for message in messages {
            if message.kind == crate::storage::PeerMessageKind::WakeRequest {
                continue;
            }
            self.messages.insert(message.id, message);
            merged = true;
        }
        if merged {
            self.deadline = Instant::now() + BACKGROUND_AGENT_WAKE_DELAY;
        }
    }

    fn from_messages(
        messages: impl IntoIterator<Item = crate::storage::PeerMessage>,
    ) -> Option<Self> {
        let mut pending = Self {
            deadline: Instant::now() + BACKGROUND_AGENT_WAKE_DELAY,
            messages: BTreeMap::new(),
        };
        pending.merge(messages);
        (!pending.messages.is_empty()).then_some(pending)
    }

    fn message_ids(&self) -> Vec<i64> {
        self.messages.keys().copied().collect()
    }
}

#[derive(Debug, Default)]
struct TerminalInputHealth {
    fast_empty_polls: u8,
    busy_empty_polls: u8,
}

impl TerminalInputHealth {
    fn record_poll(&mut self, has_event: bool, elapsed: Duration, cpu_spent: Duration) -> bool {
        if has_event {
            self.fast_empty_polls = 0;
            self.busy_empty_polls = 0;
            return false;
        }

        if elapsed <= TERMINAL_FAST_EMPTY_POLL {
            self.fast_empty_polls = self.fast_empty_polls.saturating_add(1);
        } else {
            self.fast_empty_polls = 0;
        }

        // A healthy empty poll *sleeps* for its timeout (cpu ≈ 0). When the tty
        // fd hits EOF because the terminal died without a catchable signal
        // (e.g. its tmux server was SIGKILLed), mio reports the fd permanently
        // readable and crossterm burns the entire timeout re-reading EOF — an
        // empty poll whose CPU time tracks its wall time. Wall clock alone
        // cannot see this (it looks exactly like idle), which is how orphaned
        // sessions spun at 100% CPU for hours; classify by CPU share instead
        // (≥75% of the wait spent on-CPU).
        if elapsed > TERMINAL_FAST_EMPTY_POLL && cpu_spent * 4 >= elapsed * 3 {
            self.busy_empty_polls = self.busy_empty_polls.saturating_add(1);
        } else {
            self.busy_empty_polls = 0;
        }

        self.fast_empty_polls >= TERMINAL_FAST_EMPTY_POLL_LIMIT
            || self.busy_empty_polls >= TERMINAL_BUSY_EMPTY_POLL_LIMIT
    }
}

/// CPU time consumed so far by the calling thread, read around the terminal
/// poll to tell a sleeping wait from the dead-tty EOF spin. `None` when the
/// clock is unavailable; the caller treats that as zero CPU, degrading the
/// spin detector to disabled rather than risking a false positive.
fn thread_cpu_time() -> Option<Duration> {
    let ts = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).ok()?;
    Some(Duration::new(
        ts.tv_sec().max(0) as u64,
        ts.tv_nsec().clamp(0, 999_999_999) as u32,
    ))
}

/// Consecutive pre-poll hangup readings before the terminal is declared gone.
/// 3 checks ≈ 150ms at the 50ms frame cadence — enough to shrug off a spurious
/// reading, fast enough that a dead tty never gets a chance to spin.
const TTY_HANGUP_CONSECUTIVE_LIMIT: u8 = 3;
/// Cadence of the watchdog thread's tty liveness check. With the 3-reading
/// debounce, a dead terminal force-exits the process within ~1.5s.
const TTY_WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);

/// Detects the controlling terminal dying out from under the TUI *before* the
/// crossterm poll is entered. This must run pre-poll: when a pty master closes
/// without a catchable signal reaching us (e.g. the tmux server hosting the
/// pane is SIGKILLed), the slave fd reads EOF forever and crossterm's internal
/// drain loop treats `Ok(0)` as progress — `event::poll` never returns, so no
/// post-poll health check can ever observe the failure. Orphaned sessions spun
/// at 100% CPU for hours inside that loop. A zero-timeout `poll(2)` on the pty
/// slave reports `POLLHUP` at kernel level, no signal required.
///
/// The fd polled is **stdin**, not `/dev/tty`: on macOS the `/dev/tty` alias
/// device does not implement poll and reports `POLLNVAL` even on a perfectly
/// healthy terminal (verified empirically in a live pane), which this check
/// would read as a dead terminal and exit on the spot.
struct TtyHangupWatch {
    /// Whether stdin was a terminal when the watch was armed. When it is not
    /// (redirected stdin), the watch is disabled rather than risking hangup
    /// verdicts about an fd that is not the terminal crossterm reads.
    enabled: bool,
    consecutive_hangups: u8,
}

impl TtyHangupWatch {
    fn open() -> Self {
        use std::io::IsTerminal;
        Self {
            enabled: std::io::stdin().is_terminal(),
            consecutive_hangups: 0,
        }
    }

    /// Last-resort guard on its own OS thread: force-exit the process once the
    /// controlling terminal is gone. The in-loop check above only helps when
    /// the run loop is still turning — but the loop is usually *inside*
    /// `event::poll` at the moment the pty dies, and crossterm never returns
    /// from it (see [`TtyHangupWatch`]), so no in-loop code can run again. A
    /// plain thread needs no cooperation from the wedged loop.
    ///
    /// `process::exit` skips the graceful teardown (the session row stays
    /// `Active` and is promoted to `Interrupted` on the next launch — accurate
    /// for a killed terminal) and orphans any still-running background task
    /// children. That is the acceptable cost of never leaving an invisible
    /// bonsai spinning at 100% CPU for hours. Exits with 129 (128+SIGHUP), the
    /// code the default SIGHUP disposition would have produced.
    fn spawn_watchdog_thread() {
        std::thread::spawn(|| {
            let mut watch = Self::open();
            loop {
                std::thread::sleep(TTY_WATCHDOG_INTERVAL);
                if watch.poll_hangup() {
                    tracing::warn!(
                        "controlling terminal hung up and the run loop cannot observe it; force-exiting"
                    );
                    std::process::exit(129);
                }
            }
        });
    }

    /// One zero-timeout liveness check (~1µs). Returns `true` once the tty has
    /// reported hangup [`TTY_HANGUP_CONSECUTIVE_LIMIT`] times in a row.
    fn poll_hangup(&mut self) -> bool {
        use std::os::fd::AsFd;

        if !self.enabled {
            return false;
        }
        let stdin = std::io::stdin();
        // POLLIN interest is required: Linux reports POLLHUP/POLLERR even for
        // an empty interest set, but macOS reports nothing at all unless some
        // event was requested (verified empirically on a closed-master pty:
        // empty mask → no revents; POLLIN mask → POLLIN|POLLHUP). Polling
        // never consumes input, and only the hangup/error bits are examined
        // below, so pending keystrokes neither trip this check nor get eaten.
        let mut fds = [nix::poll::PollFd::new(
            stdin.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        let hung_up = match nix::poll::poll(&mut fds, nix::poll::PollTimeout::ZERO) {
            Ok(0) => false,
            Ok(_) => fds[0].revents().is_some_and(|revents| {
                revents.intersects(
                    nix::poll::PollFlags::POLLHUP
                        | nix::poll::PollFlags::POLLERR
                        | nix::poll::PollFlags::POLLNVAL,
                )
            }),
            // EINTR or the like: not evidence either way.
            Err(_) => false,
        };
        if hung_up {
            self.consecutive_hangups = self.consecutive_hangups.saturating_add(1);
        } else {
            self.consecutive_hangups = 0;
        }
        self.consecutive_hangups >= TTY_HANGUP_CONSECUTIVE_LIMIT
    }
}

fn terminal_event_requests_immediate_redraw(event: &Event) -> bool {
    matches!(event, Event::Resize(_, _))
}

/// Pointer motion arrives as a `Moved` flood under crossterm's all-motion
/// mouse capture, and nothing in the UI renders hover state (`map_mouse`
/// ignores `Moved` entirely). These events are dropped before they can
/// request a redraw — otherwise merely moving the mouse across the screen
/// multiplies the refresh rate far past the constant redraw cadence.
fn terminal_event_is_pointer_motion(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            ..
        })
    )
}

fn redraw_interval(active_work: bool) -> Duration {
    if active_work {
        ACTIVE_REDRAW_INTERVAL
    } else {
        IDLE_REDRAW_INTERVAL
    }
}

/// Whether *any* asynchronous work is in flight, so the periodic redraw runs at
/// the active (10 FPS) cadence rather than the 1s idle one. The main-agent
/// `task_state` alone is not enough: background tasks, detached subagents, and
/// interactive terminals keep animating spinners and elapsed timers on screen
/// while the agent is idle, and at the idle cadence those tick only once a
/// second (visibly janky). The app-side mirrors of these (`app.background_tasks`
/// / `app.subtasks`) are refreshed only while their modal is open, so the live
/// answer has to come from the registries. Called at most once per drawn frame
/// (≤10×/sec); each registry lock is short-held and uncontended.
async fn any_task_active(
    app: &AppState,
    tasks: &TaskController,
    subagents: &crate::subagent::SubagentRegistry,
    background_tasks: &BackgroundTaskRegistry,
    terminals: &TerminalRegistry,
) -> bool {
    app.task_state.is_busy()
        || tasks.is_busy()
        || subagents.has_active_detached()
        || background_tasks.has_running().await
        || terminals.has_running().await
}

fn should_draw_frame(redraw_requested: bool, now: Instant, next_redraw_at: Instant) -> bool {
    redraw_requested || now >= next_redraw_at
}

fn subagent_event_requests_redraw(
    event: crate::subagent::SubagentEvent,
    subtask_list_open: bool,
) -> bool {
    subtask_list_open || matches!(event, crate::subagent::SubagentEvent::Finished)
}

fn draw_tui_frame(terminal: &mut TerminalSession, app: &mut AppState) -> Result<()> {
    let terminal_area = terminal.terminal_mut().size()?.into();
    clamp_scrolls(app, terminal_area);

    terminal
        .terminal_mut()
        .draw(|f| crate::tui::view::draw(f, app))?;

    let title = crate::tui::view::terminal_title(app);
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::SetTitle(title.as_str()));
    Ok(())
}

fn should_auto_open_plan_findings(
    app: &AppState,
    previous_finding_count: usize,
    next_finding_count: usize,
    active_agent_mode: Option<AgentMode>,
) -> bool {
    matches!(app.view, View::Agent)
        && matches!(active_agent_mode, Some(AgentMode::Review))
        && next_finding_count > previous_finding_count
}

fn sync_plan_store_snapshot(
    app: &mut AppState,
    plan: &crate::plan::PlanDoc,
    active_agent_mode: Option<AgentMode>,
) -> bool {
    if plan.revision == app.plan.revision {
        return false;
    }

    let previous_finding_count = app.plan.findings.len();
    let next_finding_count = plan.findings.len();
    let auto_open_findings = should_auto_open_plan_findings(
        app,
        previous_finding_count,
        next_finding_count,
        active_agent_mode,
    );

    app.plan = plan.clone();
    // Plan content changed (new task, new section, etc.) — follow it to the
    // bottom so the user always sees the latest edit. The render path clamps
    // this against the viewport's max_scroll, so over-shooting is safe.
    app.reduce(AppAction::SetPlanScroll(u16::MAX));

    if auto_open_findings {
        app.reduce(AppAction::SetView(View::Plan));
        app.reduce(AppAction::SetFocus(Focus::Plan));
    }
    true
}

fn provider_selection_to_persist(
    app: &AppState,
    event: &RuntimeEvent,
) -> Option<ProviderRunSelection> {
    let RuntimeEvent::CommandFinished(command) = event else {
        return None;
    };
    let CommandOutcomeEvent::Applied {
        generation,
        provider,
        ..
    } = command.as_ref();
    if generation.is_some_and(|generation| generation != app.command_generation()) {
        return None;
    }
    provider.as_deref().cloned()
}

#[derive(Debug, Clone, Copy)]
struct FirstRunRuntimeTransition {
    step: Option<crate::onboarding::FirstRunStep>,
    command_finished: bool,
    command_generation: Option<u64>,
    provider_selection_applied: bool,
    model_selection_pending: Option<u64>,
    agent_completed: bool,
}

impl FirstRunRuntimeTransition {
    fn capture(app: &AppState, event: &RuntimeEvent) -> Self {
        let command = match event {
            RuntimeEvent::CommandFinished(outcome)
            | RuntimeEvent::CatalogReloaded { outcome, .. } => Some(outcome.as_ref()),
            _ => None,
        };
        let provider_selection_applied = command.is_some_and(|command| {
            matches!(
                command,
                CommandOutcomeEvent::Applied {
                    provider: Some(_),
                    ..
                }
            )
        });
        let command_generation = command.and_then(|command| match command {
            CommandOutcomeEvent::Applied { generation, .. } => *generation,
        });
        Self {
            step: app.first_run_step,
            command_finished: command.is_some(),
            command_generation,
            provider_selection_applied,
            model_selection_pending: app.first_run_model_selection_pending,
            agent_completed: matches!(
                event,
                RuntimeEvent::AgentFinished(Ok(crate::tui::event::AgentRunOutcome::Completed))
            ),
        }
    }
}

async fn apply_first_run_runtime_transition(
    app: &mut AppState,
    transition: FirstRunRuntimeTransition,
    storage: &Storage,
    registry: &ProviderRegistry,
    session_store: &Arc<Mutex<SessionStore>>,
) {
    let matching_model_command = transition.model_selection_pending.is_some()
        && transition.model_selection_pending == transition.command_generation;
    if matching_model_command {
        app.first_run_model_selection_pending = None;
    }

    match transition.step {
        Some(crate::onboarding::FirstRunStep::Provider) if transition.command_finished => {
            let session = session_store.lock().await;
            let authorized = registry.all().iter().any(|factory| {
                let provider_id = factory.metadata().id.as_ref();
                factory.is_authorized(session.session(provider_id))
            });
            drop(session);
            if authorized {
                crate::tui::runtime_actions::open_first_run_step(
                    app,
                    crate::onboarding::FirstRunStep::Model,
                );
            }
        }
        Some(crate::onboarding::FirstRunStep::Model)
            if matching_model_command && transition.provider_selection_applied =>
        {
            match storage
                .mark_first_run_checkpoint(crate::onboarding::FirstRunCheckpoint::ModelConfirmed)
                .await
            {
                Ok(()) => crate::tui::runtime_actions::open_first_run_step(
                    app,
                    crate::onboarding::FirstRunStep::WorkspaceTrust,
                ),
                Err(error) => {
                    push_command_message(
                        app,
                        CommandOutputKind::Error,
                        &format!("Failed to save first-run model selection: {error:#}"),
                    );
                    crate::tui::runtime_actions::open_first_run_step(
                        app,
                        crate::onboarding::FirstRunStep::Model,
                    );
                }
            }
        }
        Some(crate::onboarding::FirstRunStep::FirstPrompt) if transition.agent_completed => {
            match storage
                .mark_first_run_checkpoint(crate::onboarding::FirstRunCheckpoint::Completed)
                .await
            {
                Ok(()) => {
                    app.first_run_step = None;
                    push_command_message(
                        app,
                        CommandOutputKind::Status,
                        "First-run setup complete. Bonsai is ready.",
                    );
                }
                Err(error) => push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("First run succeeded, but setup state could not be saved: {error:#}"),
                ),
            }
        }
        _ => {}
    }
}

fn runtime_event_is_stale_command(app: &AppState, event: &RuntimeEvent) -> bool {
    let outcome = match event {
        RuntimeEvent::CommandFinished(outcome) | RuntimeEvent::CatalogReloaded { outcome, .. } => {
            outcome
        }
        _ => return false,
    };
    let CommandOutcomeEvent::Applied { generation, .. } = outcome.as_ref();
    generation.is_some_and(|generation| generation != app.command_generation())
}

async fn persist_provider_selection(
    storage: &Storage,
    session_id: SessionId,
    selection: Option<ProviderRunSelection>,
) {
    let Some(selection) = selection else {
        return;
    };
    if let Err(err) = storage
        .set_session_run_selection(
            session_id,
            &selection.provider,
            &selection.model,
            selection.reasoning,
        )
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            provider = %selection.provider,
            model = %selection.model,
            error = %err,
            "failed to persist active session model selection"
        );
    }
}

/// Build the repository map on a blocking thread (tree walk + tree-sitter) and
/// hand it back through `runtime_sender` so startup is never blocked on it. A
/// non-empty map arrives as `RuntimeEvent::RepoMapReady`; an empty map (no Rust
/// sources) or a build failure is silently dropped from the event path.
///
/// The returned watch resolves to `Some` for *every* outcome — empty maps and
/// failures included — so [`RepoMapInjector::apply_before_turn`]'s bounded
/// first-turn wait returns as soon as the build settles instead of timing out
/// on repos that have no map.
fn spawn_repo_map_build(
    project_root: std::path::PathBuf,
    runtime_sender: mpsc::UnboundedSender<RuntimeEvent>,
) -> tokio::sync::watch::Receiver<Option<String>> {
    let (map_sender, map_watch) = tokio::sync::watch::channel(None);
    tokio::spawn(async move {
        match tokio::task::spawn_blocking(move || crate::tool::build_repo_map(&project_root)).await
        {
            Ok(map) => {
                let _ = map_sender.send(Some(map.clone()));
                if !map.is_empty() {
                    let _ = runtime_sender.send(RuntimeEvent::RepoMapReady(map));
                }
            }
            Err(err) => {
                let _ = map_sender.send(Some(String::new()));
                tracing::warn!(error = %err, "repo map build task failed");
            }
        }
    });
    map_watch
}

/// Hands the async-built repo map to the agent. The map is stashed from
/// `RuntimeEvent::RepoMapReady` and normally applied from the per-frame idle
/// poll; turn-start sites drain it deterministically so a ready map lands in
/// the cacheable prefix *before* the request that could cache it, instead of
/// shifting the prefix between turns.
struct RepoMapInjector {
    /// Map stashed from `RuntimeEvent::RepoMapReady` until the agent is free.
    pending: Option<String>,
    /// Resolves to `Some` when the async build settles (see
    /// [`spawn_repo_map_build`]).
    watch: tokio::sync::watch::Receiver<Option<String>>,
    /// The bounded build-wait runs once, on the first turn-start only.
    first_turn_wait_done: bool,
    /// Set once a non-empty map reached the agent; later calls skip the
    /// re-apply (and its map clone) that `set_repo_map` would no-op anyway.
    applied: bool,
}

impl RepoMapInjector {
    fn new(watch: tokio::sync::watch::Receiver<Option<String>>) -> Self {
        Self {
            pending: None,
            watch,
            first_turn_wait_done: false,
            applied: false,
        }
    }

    /// Stash a map from `RuntimeEvent::RepoMapReady`; applied below via
    /// try_lock so a running turn never blocks the loop on the agent mutex.
    fn stash(&mut self, map: String) {
        self.pending = Some(map);
    }

    /// Per-frame idle site: inject a stashed map as soon as the agent is free.
    /// `try_lock` so a running turn never stalls the loop; the per-frame poll
    /// (≤50ms) retries on the next tick when the lock is contended. Returns a
    /// fresh context report when the map was applied.
    fn try_apply_when_idle(
        &mut self,
        agent: &Arc<Mutex<Agent>>,
    ) -> Option<crate::agent::ContextReport> {
        self.pending.as_ref()?;
        let Ok(mut guard) = agent.try_lock() else {
            return None;
        };
        let map = self.pending.take()?;
        guard.set_repo_map(map);
        self.applied = true;
        Some(guard.context_report())
    }

    /// Turn-start site: apply whatever map is ready before the run task takes
    /// the agent. On the very first turn only, bound-wait for a still-running
    /// build (`REPO_MAP_FIRST_TURN_WAIT`); when the build is slower than that,
    /// proceed without the map. Callers run at idle, so awaiting the agent
    /// lock is immediate. Returns a fresh context report when the map was
    /// applied.
    async fn apply_before_turn(
        &mut self,
        agent: &Arc<Mutex<Agent>>,
    ) -> Option<crate::agent::ContextReport> {
        if self.applied {
            return None;
        }
        if !self.first_turn_wait_done {
            self.first_turn_wait_done = true;
            if self.watch.borrow().is_none() {
                let _ = tokio::time::timeout(
                    REPO_MAP_FIRST_TURN_WAIT,
                    self.watch.wait_for(|map| map.is_some()),
                )
                .await;
            }
        }
        let map = self
            .pending
            .take()
            .or_else(|| self.watch.borrow().clone().filter(|map| !map.is_empty()))?;
        let mut guard = agent.lock().await;
        guard.set_repo_map(map);
        self.applied = true;
        Some(guard.context_report())
    }
}

/// Poll the current git branch on an interval and push *changes* back through
/// `runtime_sender` as `RuntimeEvent::BranchChanged`. The branch is read once at
/// startup, but the user can `git checkout`/`switch` from another terminal mid
/// session — without this, the header would show a stale branch for the rest of
/// the run. `git_branch()` is a blocking `Command`, so each poll runs on the
/// blocking pool to keep the async runtime free. An unchanged branch emits
/// nothing; the task ends when the receiver is gone (app shutting down).
fn spawn_branch_watcher(
    project_root: std::path::PathBuf,
    initial: Option<String>,
    runtime_sender: mpsc::UnboundedSender<RuntimeEvent>,
) {
    tokio::spawn(async move {
        let mut current = initial;
        let mut ticker = tokio::time::interval(BRANCH_REFRESH_INTERVAL);
        // The first tick fires immediately; drop it so the first real poll
        // happens one interval after startup (the branch is already known).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let root = project_root.clone();
            let next = match tokio::task::spawn_blocking(move || git_branch(&root)).await {
                Ok(branch) => branch,
                Err(err) => {
                    tracing::warn!(error = %err, "branch watcher poll failed");
                    continue;
                }
            };
            if next != current {
                current = next.clone();
                if runtime_sender
                    .send(RuntimeEvent::BranchChanged(next))
                    .is_err()
                {
                    break;
                }
            }
        }
    });
}

/// Poll the shared store for inter-agent messages addressed to this session
/// and emit them as [`RuntimeEvent::PeerMessagesArrived`]. The claim
/// is exactly-once (`delivered_ui_at_ms` guard), so a message renders a single
/// blue chat message no matter how pollers race. Interval matches the branch
/// watcher; ends when the receiver is gone.
/// Sweep verdict for this session's pending waits (pure, unit-tested): which
/// waited-on targets can no longer fire their wake and must be expired.
/// A target *missing* from the live-peer set (crashed or exited without
/// firing) expires immediately. A target *present but idle* two consecutive
/// ticks expires too — an idle target has no run whose end could fire the
/// wake (it parked itself, or its fire was lost); this is also what breaks
/// 3+-party wake cycles, since a parked session heartbeats idle. The two-tick
/// debounce absorbs the run-finished → done-notice-claimed race. A working
/// target resets its counter.
fn expired_wake_targets(
    waiting_on: &[crate::storage::SessionId],
    live_working: &std::collections::HashMap<crate::storage::SessionId, bool>,
    idle_ticks: &mut std::collections::HashMap<crate::storage::SessionId, u8>,
) -> Vec<crate::storage::SessionId> {
    // Waits that resolved on their own (fired, or expired elsewhere) must not
    // leave stale counters behind.
    idle_ticks.retain(|target, _| waiting_on.contains(target));
    let mut expired = Vec::new();
    for target in waiting_on {
        match live_working.get(target) {
            None => {
                idle_ticks.remove(target);
                expired.push(*target);
            }
            Some(true) => {
                idle_ticks.remove(target);
            }
            Some(false) => {
                let ticks = idle_ticks.entry(*target).or_insert(0);
                *ticks += 1;
                if *ticks >= 2 {
                    idle_ticks.remove(target);
                    expired.push(*target);
                }
            }
        }
    }
    expired
}

fn spawn_peer_watcher(
    peer_bus: Arc<crate::peer::PeerBus>,
    runtime_sender: mpsc::UnboundedSender<RuntimeEvent>,
) {
    const PEER_POLL_INTERVAL: Duration = Duration::from_secs(2);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PEER_POLL_INTERVAL);
        ticker.tick().await;
        // Last overview snapshot, so `PeersChanged` fires only on an actual
        // change (branch-watcher discipline) — an idle heartbeat every tick
        // must not repaint the `/peers` view.
        let mut last_overview: Option<Vec<crate::peer::PeerOverview>> = None;
        // Last parked-inbox count, so the composer badge only repaints on an
        // actual change.
        let mut last_pending: Option<usize> = None;
        // Consecutive idle observations per waited-on target (the sweep's
        // two-tick debounce state).
        let mut idle_ticks: std::collections::HashMap<crate::storage::SessionId, u8> =
            std::collections::HashMap::new();
        loop {
            ticker.tick().await;
            if runtime_sender.is_closed() {
                break;
            }
            // Stranded-waiter sweep: expire waits whose target can no longer
            // fire them, *before* the inbox claim so the synthesized done
            // notice is delivered on this same tick.
            match peer_bus.wake_relationships().await {
                Ok(relationships) if !relationships.waiting_on.is_empty() => {
                    let live_working = match peer_bus.live_peers().await {
                        Ok(peers) => peers
                            .into_iter()
                            .map(|peer| (peer.id, peer.working))
                            .collect(),
                        Err(err) => {
                            tracing::debug!(
                                error = %format!("{err:#}"),
                                "peer liveness read failed; skipping wake sweep"
                            );
                            continue;
                        }
                    };
                    for target in expired_wake_targets(
                        &relationships.waiting_on,
                        &live_working,
                        &mut idle_ticks,
                    ) {
                        match peer_bus.expire_wake_subscription(target).await {
                            Ok(true) => {
                                tracing::info!(%target, "expired unfireable peer wake");
                            }
                            Ok(false) => {}
                            Err(err) => {
                                tracing::debug!(
                                    error = %format!("{err:#}"),
                                    %target,
                                    "peer wake expiry failed"
                                );
                            }
                        }
                    }
                }
                Ok(_) => idle_ticks.clear(),
                Err(err) => {
                    tracing::debug!(error = %format!("{err:#}"), "wake relationship read failed");
                }
            }
            match peer_bus.claim_ui_undelivered().await {
                Ok(messages) if !messages.is_empty() => {
                    if runtime_sender
                        .send(RuntimeEvent::PeerMessagesArrived(messages))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::debug!(error = %format!("{err:#}"), "peer watcher poll failed");
                }
            }
            // Presence snapshot for the `/peers` view.
            if let Ok(overview) = peer_bus.overview().await
                && last_overview.as_ref() != Some(&overview)
            {
                last_overview = Some(overview.clone());
                if runtime_sender
                    .send(RuntimeEvent::PeersChanged(overview))
                    .is_err()
                {
                    break;
                }
            }
            // Parked-inbox badge: messages that arrived but await injection on
            // the next turn. Drops back to 0 once a turn drains them.
            if let Ok(pending) = peer_bus.pending_inbox_count().await {
                let pending = pending.max(0) as usize;
                if last_pending != Some(pending) {
                    last_pending = Some(pending);
                    if runtime_sender
                        .send(RuntimeEvent::PeerInboxChanged(pending))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

/// Whether a due peer wake may pass the policy brakes (pure, unit-tested).
/// A *solicited* wake — the arming batch carried a done notice, which only
/// ever exists for a park this session entered itself via `wake_when_done` —
/// resumes in-flight work rather than starting new work, so it bypasses the
/// autonomy gate, the hop cap (enforced at subscription creation instead;
/// refusing here would strand an already-parked waiter forever), and the
/// wake budget (parks bound it naturally). Every tool call after the resume
/// is still approval-gated. Unsolicited wakes keep every brake.
fn peer_wake_gate_allows(solicited: bool, approval: crate::tool::ApprovalLevel, hop: i64) -> bool {
    solicited
        || (approval >= crate::tool::ApprovalLevel::Balanced && hop < crate::peer::MAX_PEER_HOPS)
}

fn peer_wake_authority(
    messages: &[crate::storage::PeerMessage],
    waiting_for_peer: Option<crate::agent::PeerWait>,
) -> Option<(i64, bool)> {
    let hop = messages.iter().map(|message| message.hop_count).max()?;
    let solicited = waiting_for_peer.is_some_and(|wait| {
        messages.iter().any(|message| {
            message.kind == crate::storage::PeerMessageKind::DoneNotice
                && message.from_session_id == wait.session_id
                && message.wake_subscription_id == Some(wait.subscription_id)
        })
    });
    Some((hop, solicited))
}

fn update_peer_wait_recheck(
    pending: &mut Option<PendingPeerWaitRecheck>,
    event: &RuntimeEvent,
    now: Instant,
) {
    match event {
        RuntimeEvent::AgentFinished(Ok(crate::tui::event::AgentRunOutcome::Waiting(
            crate::agent::WaitReason::Peer(wait),
        ))) => *pending = Some(PendingPeerWaitRecheck::new(*wait, now)),
        RuntimeEvent::AgentStarted | RuntimeEvent::AgentFinished(_) => *pending = None,
        _ => {}
    }
}

async fn maybe_recheck_peer_wait(
    pending: &mut Option<PendingPeerWaitRecheck>,
    app: &AppState,
    tasks: &TaskController,
    peer_bus: &Arc<crate::peer::PeerBus>,
) {
    let Some(recheck) = *pending else {
        return;
    };
    if app.waiting_for_peer != Some(recheck.wait) || app.task_state.is_busy() || tasks.is_busy() {
        *pending = None;
        return;
    }
    if Instant::now() < recheck.deadline {
        return;
    }

    match peer_bus.recheck_wake_subscription(recheck.wait).await {
        Ok(true) => {
            *pending = None;
            tracing::info!(
                target = %recheck.wait.session_id,
                subscription_id = recheck.wait.subscription_id,
                "resuming peer wait for periodic recheck"
            );
        }
        Ok(false) => *pending = None,
        Err(err) => {
            tracing::debug!(
                error = %format!("{err:#}"),
                target = %recheck.wait.session_id,
                subscription_id = recheck.wait.subscription_id,
                "peer wake recheck failed"
            );
        }
    }
}

/// Start a turn on the idle agent because peer messages arrived,
/// mirroring [`maybe_start_background_wake`] with the anti-loop brakes on top:
/// the policy gate ([`peer_wake_gate_allows`]), the actionable-inbox re-check
/// (never start a turn with nothing left to inject), and — for unsolicited
/// wakes — the rolling-hour wake budget.
#[allow(clippy::too_many_arguments)]
async fn maybe_start_peer_wake(
    pending_peer_wake: &mut Option<PendingPeerWake>,
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    peer_bus: &Arc<crate::peer::PeerBus>,
) -> bool {
    let Some(deadline) = pending_peer_wake.as_ref().map(|pending| pending.deadline) else {
        return false;
    };
    if Instant::now() < deadline {
        return false;
    }
    // A busy agent keeps the wake pending: the loop-top injection already
    // delivers the messages into the running turn, and the arming stays live
    // in case that turn ends before the claim happens.
    if app.task_state.is_busy() || tasks.is_busy() {
        return false;
    }

    let message_ids = pending_peer_wake
        .as_ref()
        .map(PendingPeerWake::message_ids)
        .unwrap_or_default();
    let durable_messages = match peer_bus.pending_agent_messages_by_id(&message_ids).await {
        Ok(messages) => messages,
        Err(err) => {
            tracing::debug!(error = %format!("{err:#}"), "peer wake inbox read failed");
            return false;
        }
    };
    if durable_messages.is_empty() {
        *pending_peer_wake = None;
        return false;
    }
    let Some((pending_hop, solicited)) =
        peer_wake_authority(&durable_messages, app.waiting_for_peer)
    else {
        *pending_peer_wake = None;
        return false;
    };
    if !peer_wake_gate_allows(solicited, app.approval_level, pending_hop) {
        *pending_peer_wake = None;
        return false;
    }
    if !solicited && !peer_bus.try_consume_wake_slot() {
        *pending_peer_wake = None;
        return false;
    }

    *pending_peer_wake = None;
    app.waiting_for_peer = None;
    peer_bus.begin_turn(crate::peer::TurnOrigin::PeerWake { hop: pending_hop });
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(Instant::now());
    app.reduce(AppAction::Agent(UiEvent::Thinking(
        "Peer messages arrived; resuming agent".to_string(),
    )));
    // Resume under the session's live persona (see the background-wake site):
    // a hardcoded Coding here would flip a plan-view session into a mutating
    // coding run on a mere peer message.
    if let Err(err) = tasks.start_agent_resume(agent, sink, app.active_mode()) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
    }
    true
}

/// If `command` clears the transcript (`/clear`, `/new`), flush the outgoing
/// session's dirty app snapshots, clear the plan, rotate to a fresh persisted
/// session, and reset the snapshot signatures. Shared by the top-of-loop
/// `CommandFinished` handling and the `CatalogReloaded`-derived `CommandFinished`
/// so neither path can clear the in-app transcript without also rotating the
/// persisted session — otherwise a `clear_transcript: true` catalog-reload
/// outcome would strand the outgoing session's edits.
#[allow(clippy::too_many_arguments)]
async fn rotate_persisted_session_on_clear(
    command: &CommandOutcomeEvent,
    storage: &Storage,
    active_session_id: &SharedActiveSessionId,
    project_root: &Path,
    plan_store: &SharedPlanStore,
    app: &mut AppState,
    current_session_id: &mut SessionId,
    persisted_signatures: &mut PersistedSnapshotSignatures,
    agent: &Arc<Mutex<Agent>>,
) {
    if !matches!(
        command,
        CommandOutcomeEvent::Applied {
            clear_transcript: true,
            ..
        }
    ) {
        return;
    }

    // Flush the outgoing session's dirty transcript/plan/todo before clearing
    // app state and rotating. Otherwise edits made since the last periodic flush
    // are stranded and lost when the signatures reset below — and the old row,
    // kept for `/resume`, would be missing its final content. Signature-gated,
    // so a near-no-op when nothing changed; runs while app.* still holds the old
    // session (plan is cleared just below). NOTE: this is the *app-only* flush —
    // the `/clear` handler has already called `agent.clear()`, so re-reading the
    // live agent here would persist *cleared* context/usage onto the outgoing
    // session, wiping it. Its agent context is already current from the periodic
    // flush.
    persist_changed_app_snapshots(storage, *current_session_id, &*app, persisted_signatures).await;
    // `agent.clear()` has already closed the active episode as a hard boundary,
    // but the app-only flush above intentionally does not read cleared agent
    // context. Persist just the outgoing episode ledger, then empty the shared
    // store before assigning a new session id. Otherwise the old session keeps
    // an Active row and the new session can recall archives from its predecessor.
    let outgoing_episodes = agent.lock().await.episodes_snapshot();
    if let Some(episodes) = outgoing_episodes.as_deref()
        && let Err(err) = storage
            .replace_episodes_snapshot(*current_session_id, episodes)
            .await
    {
        tracing::warn!(
            session_id = %*current_session_id,
            error = %format!("{err:#}"),
            "failed to persist outgoing episodes during session rotation"
        );
    }
    agent.lock().await.clear_episodes_for_session_rotation();
    {
        let mut plan = plan_store.lock().await;
        plan.edit().clear();
        app.plan = plan.clone();
    }
    app.reduce(AppAction::SetActiveSavedPlan(None));
    match rotate_persisted_session(storage, *current_session_id, project_root, &*app).await {
        Ok(next_id) => {
            *current_session_id = next_id;
            let cache_key = storage.conversation_cache_key(next_id).await;
            let mut agent_guard = agent.lock().await;
            if let Ok(cache_key) = &cache_key {
                agent_guard.set_conversation_cache_key(cache_key);
            }
            agent_guard
                .set_execution_lane(crate::agent::ExecutionLane::parent(next_id.to_string()));
            drop(agent_guard);
            if let Err(err) = cache_key {
                tracing::warn!(
                    session_id = %next_id,
                    error = %format!("{err:#}"),
                    "failed to apply cache key for rotated session"
                );
            }
            if let Err(err) =
                crate::storage::activate_session_heartbeat(storage, active_session_id, next_id)
                    .await
            {
                tracing::warn!(
                    session_id = %next_id,
                    error = %format!("{err:#}"),
                    "failed to activate rotated session heartbeat"
                );
            }
            if let Err(err) = storage
                .attach_recovery_workspace_session(&app.project_root, next_id)
                .await
            {
                tracing::warn!(
                    session_id = %next_id,
                    error = %format!("{err:#}"),
                    "failed to reattach rotated recovery session"
                );
            }
            if let Ok(Some(active_session_summary)) = storage.session_summary(next_id).await {
                app.set_session_identity(
                    next_id,
                    active_session_summary.name,
                    active_session_summary.summary,
                );
            } else {
                app.clear_session_identity();
            }
            refresh_session_completion_choices(app, storage, project_root, next_id).await;
            persisted_signatures.reset_to(app.transcript.as_slice(), &app.plan, &app.todo);
        }
        Err(err) => app.reduce(AppAction::Agent(UiEvent::Error(UiError::new(
            "Persistence failed",
            format!("{err:#}"),
        )))),
    }
}

/// Read-only/shared handles [`drain_runtime_events_for_frame`] needs but never
/// reassigns, grouped so the call site isn't fourteen positional arguments.
/// One agent-lock boundary (command completion / turn end — the lock is free
/// again): apply any prompt-index refreshes deferred by `/skills` and
/// `/agents` edits made while a run held the lock, then re-read the status-bar
/// mirrors.
async fn sync_agent_state_at_lock_boundary(app: &mut AppState, agent: &Arc<Mutex<Agent>>) {
    let mut agent = agent.lock().await;
    if app.skills_index_refresh_pending {
        app.skills_index_refresh_pending = false;
        agent.refresh_skills_index();
    }
    if app.agents_index_refresh_pending {
        app.agents_index_refresh_pending = false;
        agent.refresh_agents_index();
    }
    app.refresh_agent_mirrors(&agent);
}

struct RuntimeEventDrainDeps<'a> {
    storage: &'a Storage,
    agent: &'a Arc<Mutex<Agent>>,
    active_session_id: &'a SharedActiveSessionId,
    project_root: &'a Path,
    plan_store: &'a SharedPlanStore,
    session_store: &'a Arc<Mutex<SessionStore>>,
    background_tasks: &'a Arc<BackgroundTaskRegistry>,
    terminals: &'a Arc<TerminalRegistry>,
    peer_bus: &'a Arc<crate::peer::PeerBus>,
}

/// Drain every `RuntimeEvent` enqueued so far this frame. Each event either
/// short-circuits with its own side effect (repo-map ready, a peer message,
/// a catalog reload) or falls through to the generic `Runtime` reducer path.
/// A `CatalogReloaded` outcome additionally routes through the same
/// clear/rotate handling as a bare `CommandFinished`, so `/clear`'s
/// session rotation can never be skipped depending on which path produced it.
#[allow(clippy::too_many_arguments)]
async fn drain_runtime_events_for_frame(
    runtime_receiver: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    app: &mut AppState,
    current_session_id: &mut SessionId,
    persisted_signatures: &mut PersistedSnapshotSignatures,
    model_catalog: &mut Arc<crate::model_catalog::ModelCatalog>,
    registry: &mut Arc<ProviderRegistry>,
    last_model_cache_signature: &mut u64,
    pending_peer_wake: &mut Option<PendingPeerWake>,
    pending_peer_wait_recheck: &mut Option<PendingPeerWaitRecheck>,
    repo_map: &mut RepoMapInjector,
    deps: RuntimeEventDrainDeps<'_>,
) -> bool {
    let mut changed = false;
    while let Ok(event) = runtime_receiver.try_recv() {
        // Cancellation invalidates only the matching command generation. Peer,
        // background, and other runtime events remain queued and are handled.
        if runtime_event_is_stale_command(app, &event) {
            continue;
        }
        let first_run_transition = FirstRunRuntimeTransition::capture(app, &event);
        update_peer_wait_recheck(pending_peer_wait_recheck, &event, Instant::now());
        changed = true;
        // /clear and /new reset the conversation and rotate to a fresh
        // persisted session; the plan canvas belongs to it.
        if let RuntimeEvent::CommandFinished(command) = &event {
            rotate_persisted_session_on_clear(
                command,
                deps.storage,
                deps.active_session_id,
                deps.project_root,
                deps.plan_store,
                app,
                current_session_id,
                persisted_signatures,
                deps.agent,
            )
            .await;
        }
        if matches!(event, RuntimeEvent::AgentStarted) {
            // The task controller opens the durable active-time segment before
            // invoking the agent, so timeout selection sees canonical usage.
            continue;
        }
        // AgentFinished closes one turn, not the TUI process. Keep the durable
        // session Active while its heartbeat writer owns it; shutdown, /clear,
        // and /resume are the only paths that terminate or transfer the row.
        if let RuntimeEvent::RepoMapReady(map) = event {
            repo_map.stash(map);
            continue;
        }
        if let RuntimeEvent::PeerMessagesArrived(messages) = event {
            // Inter-agent chat: each claimed message renders as
            // a blue incoming message in the conversation flow. Agent-side
            // injection is a separate claim at the loop top of the next
            // turn, so display never blocks on the agent.
            //
            // A wake_request FYI ("peer parked waiting on you; no reply
            // needed") renders and injects but never *arms* a wake — starting
            // a turn just to read it would spend the peer turn the ask/ack
            // protocol exists to avoid. A done notice marks the wake
            // *solicited*: it resumes a park this session entered itself, so
            // the gate brakes don't apply (see peer_wake_gate_allows).
            let wake_worthy_messages = messages
                .iter()
                .filter(|message| message.kind != crate::storage::PeerMessageKind::WakeRequest)
                .map(|delivery| delivery.message.clone())
                .collect::<Vec<_>>();
            for delivery in messages {
                let mut text = delivery.message.body;
                crate::redact::redact_in_place(&mut text);
                app.reduce(AppAction::PeerMessage {
                    source_message_id: Some(delivery.message.id),
                    delivery_receipt: Some(delivery.receipt),
                    session_id: delivery.message.from_session_id.as_i64(),
                    outgoing: false,
                    text,
                });
            }
            if !wake_worthy_messages.is_empty() {
                // Arm the gated auto-wake: the debounce gives a
                // burst of messages one wake instead of several.
                if let Some(pending) = pending_peer_wake.as_mut() {
                    pending.merge(wake_worthy_messages);
                } else {
                    *pending_peer_wake = PendingPeerWake::from_messages(wake_worthy_messages);
                }
            }
            continue;
        }
        if let RuntimeEvent::PeersChanged(peers) = event {
            // Live-refresh the `/peers` view if it is open.
            if app.is_peer_list_open() {
                app.reduce(AppAction::RefreshPeerList { peers });
            }
            continue;
        }
        if let RuntimeEvent::PeerInboxChanged(count) = event {
            // Surface the parked-inbox badge in the composer meta line.
            app.reduce(AppAction::PeerInboxChanged { count });
            continue;
        }
        if matches!(&event, RuntimeEvent::AgentFinished(_)) {
            // Wake-on-done: this session's run just finished; if
            // no queued input immediately continues the work, notify every
            // subscriber. Single-shot claims make a double fire impossible.
            if app.queued_inputs.is_empty() {
                let bus = deps.peer_bus.clone();
                tokio::spawn(async move {
                    match bus.fire_own_wake_subscriptions().await {
                        Ok(requesters) if !requesters.is_empty() => {
                            tracing::debug!(
                                count = requesters.len(),
                                "fired peer wake subscriptions"
                            );
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::debug!(error = %format!("{err:#}"), "wake-subscription firing failed");
                        }
                    }
                });
            }
            // Fall through: AgentFinished still drives the normal reducer path.
        }
        let refresh_after_task_removal =
            matches!(event, RuntimeEvent::BackgroundTaskRemovalFinished { .. });
        if let RuntimeEvent::CatalogReloaded {
            model_catalog: next_model_catalog,
            registry: next_registry,
            outcome,
        } = event
        {
            *model_catalog = next_model_catalog;
            *registry = next_registry;
            app.sync_provider_choices(registry);
            if let Ok(guard) = deps.session_store.try_lock() {
                app.sync_cached_model_choices(&guard, registry, Some(model_catalog));
                *last_model_cache_signature = cached_model_signature(&guard, model_catalog);
            }
            let event = RuntimeEvent::CommandFinished(outcome);
            // A catalog-reload outcome that clears the transcript must rotate
            // the persisted session on the same path as a bare
            // CommandFinished — otherwise it would clear the in-app transcript
            // without flushing/rotating and strand the outgoing edits.
            if let RuntimeEvent::CommandFinished(command) = &event {
                rotate_persisted_session_on_clear(
                    command,
                    deps.storage,
                    deps.active_session_id,
                    deps.project_root,
                    deps.plan_store,
                    app,
                    current_session_id,
                    persisted_signatures,
                    deps.agent,
                )
                .await;
            }
            let provider_selection = provider_selection_to_persist(app, &event);
            app.reduce(AppAction::Runtime(event));
            sync_agent_state_at_lock_boundary(app, deps.agent).await;
            persist_provider_selection(deps.storage, *current_session_id, provider_selection).await;
            apply_first_run_runtime_transition(
                app,
                first_run_transition,
                deps.storage,
                registry,
                deps.session_store,
            )
            .await;
            continue;
        }
        let provider_selection = provider_selection_to_persist(app, &event);
        // Command completion and turn end are the two moments agent-owned
        // state can have changed behind the mirrors' back.
        let sync_mirrors = matches!(
            event,
            RuntimeEvent::CommandFinished(_) | RuntimeEvent::AgentFinished(_)
        );
        app.reduce(AppAction::Runtime(event));
        if sync_mirrors {
            sync_agent_state_at_lock_boundary(app, deps.agent).await;
        }
        persist_provider_selection(deps.storage, *current_session_id, provider_selection).await;
        apply_first_run_runtime_transition(
            app,
            first_run_transition,
            deps.storage,
            registry,
            deps.session_store,
        )
        .await;
        if refresh_after_task_removal && app.is_task_list_open() {
            refresh_background_task_list(app, deps.background_tasks, deps.terminals).await;
        }
    }
    changed
}

pub(super) async fn run(runtime: TuiRuntime) -> Result<()> {
    let TuiRuntime {
        mut agent,
        session_store,
        mut registry,
        todo_store,
        plan_store,
        mut interaction_rx,
        interaction,
        storage,
        mut model_catalog,
        resume,
        active_session_id,
        background_tasks,
        terminals,
        subagents,
        subagent_model_overrides,
        permissions,
        domain_permissions,
        peer_bus,
        project_root,
        session_project_root,
        recovery_id,
        accessibility,
    } = runtime;
    let mut terminal = TerminalSession::enter(accessibility.screen_reader)?;
    let cwd = session_project_root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    let branch = git_branch(&project_root);
    let path_search = match crate::tui::path_search::PathSearch::start(project_root.clone()) {
        Ok(search) => Some(search),
        Err(err) => {
            tracing::warn!(error = %err, "failed to initialize fff path completion");
            None
        }
    };

    // Restore the persisted theme before the first frame renders.
    let theme = {
        let session_store = session_store.lock().await;
        session_store.theme.clone()
    };
    if !theme.is_empty() {
        crate::tui::theme::set_theme(&theme);
    }

    let (ui_sender, mut ui_receiver) = mpsc::unbounded_channel();
    let (runtime_sender, mut runtime_receiver) = mpsc::unbounded_channel();
    // Build the repository map off the startup path: a large repo's tree walk
    // must never block the first frame. It is injected into the agent's context
    // via `RuntimeEvent::RepoMapReady` once ready.
    let repo_map_watch = spawn_repo_map_build(project_root.clone(), runtime_sender.clone());
    // Keep the header's branch label in sync with `git checkout`/`switch` made
    // outside the app; the initial value was read once into `branch` above.
    spawn_branch_watcher(project_root.clone(), branch.clone(), runtime_sender.clone());
    // Inter-agent message watcher: claims inbound messages every
    // couple of seconds and surfaces them as blue chat messages.
    spawn_peer_watcher(peer_bus.clone(), runtime_sender.clone());
    // Boot-time retention for the message bus; failures are non-fatal.
    {
        let storage = storage.clone();
        tokio::spawn(async move {
            if let Err(err) = storage.purge_stale_peer_state().await {
                tracing::debug!(error = %format!("{err:#}"), "peer message purge failed");
            }
        });
    }
    // Native self-update: check for a newer signed release in the background
    // and surface at most one composer-meta notice. Startup is never blocked
    // and failures stay silent (debug log only).
    {
        let bonsai_home = storage.home_dir().to_path_buf();
        let update_config = agent.config().update.clone();
        let update_sender = runtime_sender.clone();
        tokio::spawn(async move {
            let outcome = crate::update::run_startup_update(&bonsai_home, &update_config).await;
            tracing::debug!(?outcome, "startup self-update finished");
            if let Some(notice) = crate::update::startup_notice(&outcome) {
                let _ = update_sender.send(RuntimeEvent::UpdateNotice(notice));
            }
        });
    }
    let credential_persistence_preference = storage.credential_persistence().await?;
    let credential_persistence = credential_persistence_preference.unwrap_or_default();
    let first_run_progress = storage.first_run_progress().await?;
    let workspace_project_id = storage.ensure_project(&session_project_root).await?;
    let workspace_trust =
        crate::workspace_trust::WorkspaceTrustGate::load(storage.clone(), workspace_project_id)
            .await?;
    let has_authorized_provider = {
        let session = session_store.lock().await;
        registry.all().iter().any(|factory| {
            let provider_id = factory.metadata().id.as_ref();
            factory.is_authorized(session.session(provider_id))
        })
    };
    let first_run_step = crate::onboarding::initial_step(
        first_run_progress,
        credential_persistence_preference.is_some(),
        has_authorized_provider,
        workspace_trust.needs_prompt(),
    );
    // Presentation preference, restored before the first frame like the
    // theme; a read failure degrades to the normal view rather than failing
    // startup.
    let serenity_mode = storage.serenity_mode().await.unwrap_or(false);
    // Arm the opt-in lifecycle log as early as possible: the tracing layer is
    // already installed, so flipping the gate is all "enabled" means.
    let support_log_enabled = storage.support_log_enabled().await.unwrap_or(false);
    crate::logging::set_support_log_enabled(support_log_enabled);
    let run_budget = storage.run_budget().await.unwrap_or_else(|error| {
        tracing::warn!(error = %error, "failed to restore run budget preference");
        crate::run_budget::RunBudget::default()
    });
    let mut app = {
        let guard = session_store.lock().await;
        let provider = guard.current_kind_id().to_string();
        let model = guard.current_session().model.clone();
        let reasoning = guard.current_session().reasoning;
        let session_file = SessionStore::path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|err| format!("<unavailable: {err:#}>"));
        let authorized_provider_ids = registry
            .all()
            .iter()
            .filter_map(|factory| {
                let id = factory.metadata().id.as_ref();
                factory.is_authorized(guard.session(id)).then_some(id)
            })
            .collect::<Vec<_>>()
            .join(",");
        tracing::info!(
            provider = %provider,
            model = %model,
            session_file = %session_file,
            authorized_provider_ids = %authorized_provider_ids,
            provider_state = %guard.provider_state_summary(),
            "starting TUI with persisted provider"
        );
        let mut app = AppState::new(&provider, model, cwd, branch);
        app.has_authorized_provider = has_authorized_provider;
        app.reasoning = reasoning;
        app.refresh_agent_mirrors(&agent);
        app.serenity_mode = serenity_mode;
        app.support_log_enabled = support_log_enabled;
        app.reduced_motion = accessibility.reduced_motion;
        app.screen_reader_mode = accessibility.screen_reader;
        app.run_budget = run_budget;
        app.credential_persistence = credential_persistence;
        app.provider_auth_form.credential_persistence = credential_persistence;
        app.subagent_model_overrides = subagent_model_overrides;
        // Share the agent's live custom-agent registry so the persona cycle + composer
        // label resolve custom personas (composer edits hot-swap it).
        app.custom_agents = agent.custom_agents_handle();
        // Hot-swap handles for `/skills` and `/agents`: both must open mid-run,
        // and a running turn holds the agent lock for its entire duration, so
        // their rows build from these instead of locking the agent.
        app.skills = agent.skills_handle();
        app.builtin_subagents = agent.builtin_subagent_settings_handle();
        app
    };
    // Canonicalized so it matches the resolved absolute paths tools emit, letting
    // tool cards render project paths root-relative (`src/foo.rs`).
    app.project_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.clone());
    if let Some(path_search) = path_search {
        app.set_path_search(path_search);
    }
    if let Some(step) = first_run_step {
        app.first_run_step = Some(step);
        app.reduce(AppAction::OpenModal(ModalKind::Onboarding {
            step,
            cursor: if step == crate::onboarding::FirstRunStep::CredentialStorage {
                crate::session::CredentialPersistence::ALL
                    .iter()
                    .position(|value| *value == credential_persistence)
                    .unwrap_or(0)
            } else {
                step.default_choice()
            },
        }));
    } else {
        workspace_trust.prompt_after_startup(interaction.clone(), &session_project_root);
    }
    app.sync_provider_choices(&registry);
    let mut current_session_id = {
        let guard = session_store.lock().await;
        storage
            .start_session(
                &session_project_root,
                guard.current_kind_id(),
                &guard.current_session().model,
                guard.current_session().reasoning,
            )
            .await?
    };
    let conversation_cache_key = storage.conversation_cache_key(current_session_id).await?;
    agent.set_conversation_cache_key(&conversation_cache_key);
    agent.set_execution_lane(crate::agent::ExecutionLane::parent(
        current_session_id.to_string(),
    ));
    // Correlates this process's log file (keyed by pid + start-millis) with
    // the DB session id, so `bonsai.db` forensics and log tailing meet.
    tracing::info!(
        target: "bonsai::session",
        session_id = %current_session_id,
        "session attached to this process log"
    );
    crate::storage::activate_session_heartbeat(&storage, &active_session_id, current_session_id)
        .await?;
    // Liveness heartbeat: lets concurrent bonsai processes in this project root
    // tell a live peer session from a crash leftover. The explicit
    // activation above handles the first stamp; this task keeps it fresh. The
    // busy flag (mirrored per frame below) rides along so peers see
    // working vs idle.
    let busy_flag: crate::storage::SharedBusyFlag = Default::default();
    crate::storage::spawn_heartbeat_writer(
        storage.clone(),
        active_session_id.clone(),
        busy_flag.clone(),
    );
    // Now that the new session row exists, promote any leftover `Active` rows for
    // this project to `Interrupted`: those are prior runs that never exited
    // cleanly. A clean quit leaves the prior row `Completed`, so it is skipped
    // here and produces no hint. The new session is excluded by id.
    let interrupted_sessions = match storage
        .promote_active_sessions_to_interrupted(&session_project_root, current_session_id, 5)
        .await
    {
        Ok(sessions) => sessions,
        Err(err) => {
            tracing::warn!(error = %err, "failed to check interrupted sessions");
            Vec::new()
        }
    };
    set_session_identity_or_default(
        &mut app,
        &storage,
        current_session_id,
        &session_project_root,
    )
    .await;
    refresh_session_completion_choices(
        &mut app,
        &storage,
        &session_project_root,
        current_session_id,
    )
    .await;
    if let Some(recovery_id) = recovery_id.as_ref() {
        storage
            .attach_recovery_session(recovery_id, current_session_id)
            .await?;
        app.reduce(AppAction::Agent(UiEvent::Status(format!(
            "Recovery {recovery_id}: isolated worktree; finish with `bonsai recovery inspect {recovery_id}`."
        ))));
    }
    tracing::info!(
        session_id = %current_session_id,
        database = %storage.db_path().display(),
        "started persisted session"
    );
    let mut last_model_cache_signature = {
        let guard = session_store.lock().await;
        app.sync_cached_model_choices(&guard, &registry, Some(&model_catalog));
        cached_model_signature(&guard, &model_catalog)
    };
    let yolo_mode = agent.yolo_mode();
    app.approval_level = yolo_mode.level();
    app.sandbox = Some(agent.sandbox());
    // Cloned out before the agent disappears behind its mutex: memory UI
    // (busy-path /memory, manager actions) must reach the service without the
    // agent lock, which a running turn holds for its entire duration.
    let memory = agent.memory().cloned();
    let agent = Arc::new(Mutex::new(agent));
    let sink: SharedSink = Arc::new(TuiSink {
        sender: ui_sender.clone(),
        completion_evidence: crate::completion_report::CompletionEvidenceCollector::default(),
    });
    // `RuntimeActionDeps` is assembled identically at every dispatch site from
    // the same loop-scope handles. A macro (not a closure) keeps the borrows
    // inline: `mut` handles like `agent`/`registry`/`model_catalog` are mutated
    // within the loop, so a closure holding shared borrows would not compile.
    // Defined here — after every referenced handle is bound — so macro hygiene
    // resolves each identifier to the loop-scope binding at expansion time.
    macro_rules! runtime_action_deps {
        () => {
            RuntimeActionDeps {
                interaction: interaction.clone(),
                runtime_sender: runtime_sender.clone(),
                agent: agent.clone(),
                memory: memory.clone(),
                yolo_mode: yolo_mode.clone(),
                session_store: session_store.clone(),
                registry: registry.clone(),
                model_catalog: model_catalog.clone(),
                storage: &storage,
                project_root: &project_root,
                session_project_root: &session_project_root,
                active_session_id: active_session_id.clone(),
                todo_store: todo_store.clone(),
                plan_store: plan_store.clone(),
                sink: sink.clone(),
                background_tasks: background_tasks.clone(),
                terminals: terminals.clone(),
            }
        };
    }
    let mut tasks = TaskController::new(runtime_sender.clone());
    tasks.set_max_run_duration(run_budget.max_run_duration());
    tasks.configure_session_runtime_budget(
        storage.clone(),
        active_session_id.clone(),
        run_budget.max_session_active_duration(),
    );
    let termination_requested = install_termination_signal_handler();
    TtyHangupWatch::spawn_watchdog_thread();
    let mut background_events = background_tasks.subscribe();
    let mut terminal_events = terminals.subscribe();
    let mut subagent_events = subagents.subscribe();
    let mut pending_background_wake: Option<Instant> = None;
    // Peer auto-wake: armed when inbound peer messages arrive while
    // the agent may be idle; carries the highest hop among them for the
    // anti-loop brake.
    let mut pending_peer_wake: Option<PendingPeerWake> = None;
    let mut pending_peer_wait_recheck: Option<PendingPeerWaitRecheck> = None;
    // Holds the async-built repo map until the agent is free to take the lock,
    // and drains it deterministically at turn-start sites.
    let mut repo_map = RepoMapInjector::new(repo_map_watch);
    let mut ctrl_c_prompt: Option<CtrlCExitPrompt> = None;
    let mut deferred_ui_event = None;
    let mut terminal_input_health = TerminalInputHealth::default();
    let mut tty_hangup_watch = TtyHangupWatch::open();
    let mut next_persistence_flush = Instant::now();
    let mut redraw_requested = true;
    let mut next_redraw_at = Instant::now();
    let mut last_active_persona = app.active_persona.clone();
    let mut persisted_signatures = PersistedSnapshotSignatures {
        transcript: transcript_signature(app.transcript.as_slice()),
        plan: plan_signature(&app.plan),
        todo: todo_signature(&app.todo),
        context: 0,
        context_controls: 0,
        usage: 0,
        verification: 0,
        self_review: 0,
        episodes: 0,
    };
    let mut pending_snapshot_flush: Option<tokio::task::JoinHandle<PersistenceFlushResult>> = None;
    if let Some(target) = resume {
        let mut persistence = PersistenceCommandState {
            current_session_id: &mut current_session_id,
            signatures: &mut persisted_signatures,
        };
        let deps = PersistenceCommandDeps {
            storage: &storage,
            agent: agent.clone(),
            memory: memory.clone(),
            session_store: session_store.clone(),
            registry: registry.clone(),
            model_catalog: model_catalog.clone(),
            todo_store: todo_store.clone(),
            plan_store: plan_store.clone(),
            project_root: &session_project_root,
            active_session_id: active_session_id.clone(),
        };
        match target {
            crate::cli::ResumeTarget::Latest => {
                resume_latest_prior_session(
                    &mut app,
                    deps,
                    &mut persistence,
                    "No prior sessions for this project. Starting new session.",
                )
                .await?;
            }
            crate::cli::ResumeTarget::Session(id) => {
                resume_session(
                    crate::storage::SessionId::from_raw(id),
                    &mut app,
                    deps,
                    &mut persistence,
                )
                .await?;
            }
        }
    } else {
        app.latest_context_report = Some(agent.lock().await.context_report());
        if !interrupted_sessions.is_empty() {
            // Actionable orientation hint, kept as an ephemeral banner rather than
            // a persisted transcript row so it can't pile up across resumes.
            app.set_session_hint(format_interrupted_sessions(&interrupted_sessions));
        }
    }

    // Config diagnostics: a broken `.bonsai/config.toml` entry never
    // fails startup, but it must not fail *silently* either — surface it once,
    // up front, alongside the pointer to `/config validate` for detail.
    let config_diagnostics = agent.lock().await.config().diagnostics.clone();
    if !config_diagnostics.is_empty() {
        let lines: Vec<String> = config_diagnostics.iter().map(ToString::to_string).collect();
        push_command_message(
            &mut app,
            CommandOutputKind::Status,
            &format!(
                "config: {} diagnostic(s) — see /config validate\n{}",
                lines.len(),
                lines.join("\n")
            ),
        );
    }

    let result = 'run: loop {
        // Frame budget watch: everything between here and the event poll runs
        // on the UI thread, and a slow segment freezes the app — keypresses
        // (including the Ctrl+C exit dance) queue unread until the next poll.
        // Users report that as "the app hangs"; the warn below names the
        // culprit segment instead of leaving a dark zone in the log.
        let frame_work_started = Instant::now();
        let mut frame_persist = Duration::ZERO;
        let mut frame_needs_redraw = false;

        if pending_snapshot_flush
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(handle) = pending_snapshot_flush.take()
        {
            match handle.await {
                Ok(flush) => apply_persistence_flush_result(
                    flush,
                    current_session_id,
                    &mut app,
                    &agent,
                    &mut persisted_signatures,
                ),
                Err(error) => tracing::warn!(
                    session_id = %current_session_id,
                    error = %error,
                    "periodic snapshot task failed"
                ),
            }
        }

        // A SIGTERM/SIGHUP (terminal window closed, `kill`, logout) lands here
        // within a frame and exits through bounded cleanup. It is still an
        // interruption, not a successful completion: preserve the snapshot but
        // leave the session explicitly resumable.
        if termination_requested.load(Ordering::Relaxed) {
            tracing::info!(
                session_id = %current_session_id,
                "received termination signal; exiting cleanly"
            );
            tasks.abort();
            prepare_termination_shutdown(&mut app);
            break 'run Ok(());
        }
        clear_expired_ctrl_c_prompt(&mut app, &mut ctrl_c_prompt, Instant::now());
        let mut changed_files = Vec::new();
        frame_needs_redraw |= apply_ui_events_for_frame(
            &mut app,
            &mut ui_receiver,
            &mut deferred_ui_event,
            &mut changed_files,
        );
        if !changed_files.is_empty() {
            // Record off the frame path; the upsert is tiny and low-frequency
            // (one batch per finished edit/write tool).
            let storage = storage.clone();
            let session_id = current_session_id;
            tokio::spawn(async move {
                if let Err(err) = storage
                    .record_session_file_changes(session_id, &changed_files)
                    .await
                {
                    tracing::debug!(error = %format!("{err:#}"), "file-change ledger write failed");
                }
            });
        }
        // Outgoing peer messages echo into this session's own chat as blue
        // `you → #N` messages at the send site.
        let peer_echoes = peer_bus.drain_echoes();
        if !peer_echoes.is_empty() {
            frame_needs_redraw = true;
        }
        for echo in peer_echoes {
            app.reduce(AppAction::PeerMessage {
                source_message_id: None,
                delivery_receipt: None,
                session_id: echo.to_session_id.as_i64(),
                outgoing: true,
                text: echo.body,
            });
        }

        let mut refresh_task_list = false;
        let mut wake_candidate = false;
        while let Ok(event) = background_events.try_recv() {
            frame_needs_redraw = true;
            let effect = apply_background_task_event(&mut app, event);
            refresh_task_list |= effect.refresh_task_list;
            wake_candidate |= effect.wake_candidate;
        }
        while let Ok(event) = terminal_events.try_recv() {
            frame_needs_redraw = true;
            let effect = apply_terminal_event(&mut app, event);
            refresh_task_list |= effect.refresh_task_list;
            wake_candidate |= effect.wake_candidate;
        }
        if refresh_task_list && app.is_task_list_open() {
            refresh_background_task_list(&mut app, &background_tasks, &terminals).await;
        }
        if wake_candidate
            && (background_tasks.agent_wake_ready().await || terminals.agent_wake_ready().await)
        {
            pending_background_wake = Some(Instant::now() + BACKGROUND_AGENT_WAKE_DELAY);
        }

        // Subagent activity: while `/subagents` is open, keep its snapshot
        // fresh so it updates live. Each snapshot deep-clones the run's activity
        // and result (up to ~18 KB), so only do this work when the modal is
        // actually visible — the open paths (`/subagents` command and the Alt+S
        // keybind, below) refresh on open, so a closed modal can't go stale.
        let mut subagents_changed = false;
        let mut subagent_wake_candidate = false;
        loop {
            match subagent_events.try_recv() {
                Ok(event) => {
                    subagents_changed = true;
                    // Activity only changes the /subtasks snapshot. Marking the
                    // main view dirty for every streamed token bypasses the
                    // active redraw throttle and can overwhelm slower terminals
                    // when several subagents stream concurrently. The normal
                    // active-frame deadline still advances timers at 10 FPS.
                    frame_needs_redraw |=
                        subagent_event_requests_redraw(event, app.is_subtask_list_open());
                    if matches!(event, crate::subagent::SubagentEvent::Finished) {
                        subagent_wake_candidate = true;
                    }
                }
                // A burst that overran the 256-slot buffer still means the list
                // changed; treat the lag as a change so we don't skip the refresh.
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    subagents_changed = true;
                    subagent_wake_candidate = true;
                    frame_needs_redraw |= app.is_subtask_list_open();
                }
                Err(_) => break,
            }
        }
        if subagents_changed {
            // Runs adopt their model onto the launching `agent` tool call via a
            // cheap ids+models projection; the full snapshot list (which clones
            // every run's activity) is still only taken while /subagents is open.
            frame_needs_redraw |= app.adopt_subagent_models(&subagents.tool_call_models());
            if app.is_subtask_list_open() {
                app.reduce(AppAction::RefreshSubtaskList {
                    subtasks: subagents.list(),
                });
            }
        }
        if subagent_wake_candidate && subagents.agent_wake_ready() {
            pending_background_wake = Some(Instant::now() + BACKGROUND_AGENT_WAKE_DELAY);
        }

        frame_needs_redraw |= drain_runtime_events_for_frame(
            &mut runtime_receiver,
            &mut app,
            &mut current_session_id,
            &mut persisted_signatures,
            &mut model_catalog,
            &mut registry,
            &mut last_model_cache_signature,
            &mut pending_peer_wake,
            &mut pending_peer_wait_recheck,
            &mut repo_map,
            RuntimeEventDrainDeps {
                storage: &storage,
                agent: &agent,
                active_session_id: &active_session_id,
                project_root: &session_project_root,
                plan_store: &plan_store,
                session_store: &session_store,
                background_tasks: &background_tasks,
                terminals: &terminals,
                peer_bus: &peer_bus,
            },
        )
        .await;
        // ToolStarted and SubagentEvent travel over separate channels. Reconcile
        // after every runtime drain so a completion observed on an earlier frame
        // still closes a transcript row that arrives later.
        frame_needs_redraw |= apply_completed_subagent_tool_calls(&mut app, &subagents);
        // Inject the async-built repo map as soon as the agent is free.
        if let Some(report) = repo_map.try_apply_when_idle(&agent) {
            app.latest_context_report = Some(report);
            frame_needs_redraw = true;
        }
        // Exit the loop as soon as a runtime event (e.g. a CommandFinished
        // with quit:true from /quit, or the runtime-reducer arm) flips us to
        // Exiting, so we skip one final draw before breaking below.
        if matches!(app.task_state, TaskState::Exiting) {
            tracing::info!(
                pending_task = tasks.is_busy(),
                "TUI exiting; run loop broken"
            );
            break 'run Ok(());
        }
        // Drive phase auto-advance from this unconditional per-frame site, not
        // only the poll_finished site. The AgentFinished reducer sets
        // phase_advance + Idle at the top-of-loop drain, which can land a frame
        // *after* poll_finished already reaped (take()s) the run handle — so a
        // poll_finished-only driver would miss the signal and the phased plan
        // would stall after the first phase. maybe_advance self-gates on
        // idle + !is_busy + phase_advance.take(), so it is a cheap no-op
        // otherwise. Phase advance takes priority over a queued run, mirroring
        // the poll_finished site (and how start_pending runs at both sites).
        let started_deferred = start_next_deferred_command_if_idle(
            &mut app,
            &mut tasks,
            agent.clone(),
            session_store.clone(),
            &project_root,
            DeferredCommandDeps {
                registry: registry.clone(),
                model_catalog: model_catalog.clone(),
                storage: Some(&storage),
            },
        )
        .await;
        frame_needs_redraw |= started_deferred;
        if !started_deferred {
            // A confirmed plan-mode switch continues the just-finished turn under
            // the new persona; it takes priority over phase-advance and queued
            // runs for the idle slot (the two are mutually exclusive in practice
            // — a switch only follows a flat coding run, never a phased plan).
            let switched = maybe_apply_persona_switch(
                &mut app,
                &mut tasks,
                agent.clone(),
                sink.clone(),
                &mut repo_map,
            )
            .await;
            frame_needs_redraw |= switched;
            let advanced = !switched
                && maybe_advance_plan_phase(
                    &mut app,
                    &mut tasks,
                    agent.clone(),
                    sink.clone(),
                    todo_store.clone(),
                    plan_store.clone(),
                    &mut repo_map,
                )
                .await;
            frame_needs_redraw |= advanced;
            if !switched && !advanced {
                frame_needs_redraw |= start_pending_queued_run_if_idle(
                    &mut app,
                    &mut tasks,
                    agent.clone(),
                    sink.clone(),
                    &mut repo_map,
                    &registry,
                    &model_catalog,
                )
                .await;
            }
        }
        frame_needs_redraw |= maybe_start_background_wake(
            &mut pending_background_wake,
            &mut app,
            &mut tasks,
            agent.clone(),
            sink.clone(),
            &background_tasks,
            &terminals,
            &subagents,
            &peer_bus,
        )
        .await;
        frame_needs_redraw |= maybe_start_peer_wake(
            &mut pending_peer_wake,
            &mut app,
            &mut tasks,
            agent.clone(),
            sink.clone(),
            &peer_bus,
        )
        .await;
        maybe_recheck_peer_wait(&mut pending_peer_wait_recheck, &app, &tasks, &peer_bus).await;

        frame_needs_redraw |= apply_interaction_request(&mut app, &mut interaction_rx);

        app.reduce(AppAction::Tick);
        // A growing bonsai animates at the poll cadence; without this the
        // idle loop repaints once a second and the growth would stutter.
        if app.bonsai_growing() {
            frame_needs_redraw = true;
        }
        if let Ok(guard) = todo_store.try_lock() {
            // Only re-clone when the todo list actually changed; the common
            // idle frame leaves it untouched and pays no allocation.
            if guard.todos() != app.todo.as_slice() {
                app.todo = guard.todos().to_vec();
                frame_needs_redraw = true;
                // The model rewrote the list: stop browsing, follow the active
                // phase again, and bring its in-progress item into view at clamp
                // time (resolved once the sidebar size is known).
                app.todo_phase_view = None;
                app.scroll_todo_in_progress_into_view = true;
            }
        }
        if let Ok(plan) = plan_store.try_lock() {
            frame_needs_redraw |=
                sync_plan_store_snapshot(&mut app, &plan, tasks.active_agent_mode());
        }

        // Refresh the read-only completion snapshots without taking the session
        // lock on every keystroke. We use try_lock so a foreground command
        // already holding the lock cannot block the event loop and wedge Ctrl+C.
        if let Ok(guard) = session_store.try_lock() {
            let signature = cached_model_signature(&guard, &model_catalog);
            if signature != last_model_cache_signature {
                app.sync_cached_model_choices(&guard, &registry, Some(&model_catalog));
                last_model_cache_signature = signature;
                frame_needs_redraw = true;
            }
        }

        let now = Instant::now();
        if now >= next_persistence_flush && pending_snapshot_flush.is_none() {
            let persist_started = Instant::now();
            pending_snapshot_flush = Some(spawn_changed_snapshot_flush(
                &storage,
                current_session_id,
                &app,
                &agent,
                persisted_signatures,
            ));
            frame_persist = persist_started.elapsed();
            next_persistence_flush = now + PERSISTENCE_FLUSH_INTERVAL;
        }

        // Mirror working/idle into the heartbeat's shared flag so
        // concurrent sessions see availability without a message round-trip.
        busy_flag.store(
            app.task_state.is_busy(),
            std::sync::atomic::Ordering::Relaxed,
        );

        // Mirror the in-flight run's persona for the composer meta line: when
        // it differs from the selected persona the line renders the pending
        // switch (`Running → Selected`) instead of silently relabeling a run
        // that is still executing under its dispatch-time persona.
        let running_persona = tasks.active_agent_persona().cloned();
        if app.running_persona != running_persona {
            app.running_persona = running_persona;
            redraw_requested = true;
        }

        if frame_needs_redraw {
            redraw_requested = true;
        }
        let mut frame_draw = Duration::ZERO;
        let now = Instant::now();
        if should_draw_frame(redraw_requested, now, next_redraw_at) {
            let draw_started = Instant::now();
            draw_tui_frame(&mut terminal, &mut app)?;
            frame_draw = draw_started.elapsed();
            redraw_requested = false;
            next_redraw_at = Instant::now()
                + redraw_interval(
                    any_task_active(&app, &tasks, &subagents, &background_tasks, &terminals).await,
                );
        }

        if matches!(app.task_state, TaskState::Exiting) {
            tracing::info!(
                pending_task = tasks.is_busy(),
                "TUI exiting; run loop broken"
            );
            break 'run Ok(());
        }

        // Liveness-check the tty BEFORE entering the crossterm poll: with the
        // terminal dead, `event::poll` can spin on EOF internally and never
        // return, so this is the last point where the loop can still exit.
        if tty_hangup_watch.poll_hangup() {
            tracing::warn!(
                consecutive_hangups = tty_hangup_watch.consecutive_hangups,
                "controlling terminal hung up; exiting TUI"
            );
            tasks.abort();
            app.reduce(AppAction::SetTaskState(TaskState::Exiting));
            break 'run Ok(());
        }

        // Name the culprit when a frame's work starved the input path. 500ms is
        // 10 poll periods — users feel it as a hang, and a queued Ctrl+C exit
        // sits unread the whole time.
        let frame_work = frame_work_started.elapsed();
        if frame_work >= SLOW_FRAME_WARN_THRESHOLD {
            tracing::warn!(
                frame_work_ms = frame_work.as_millis() as u64,
                persist_ms = frame_persist.as_millis() as u64,
                draw_ms = frame_draw.as_millis() as u64,
                task_state = ?app.task_state,
                "slow UI frame; input (incl. Ctrl+C) was starved this long"
            );
        }

        let poll_started_at = Instant::now();
        // CPU-time bracket around the blocking poll (no await between the two
        // readings, so both observe the same OS thread).
        let poll_cpu_started = thread_cpu_time();
        let has_terminal_event = match event::poll(EVENT_POLL_TIMEOUT) {
            Ok(has_event) => has_event,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => false,
            Err(err) => {
                tracing::warn!(error = %err, "terminal event poll failed; exiting TUI");
                tasks.abort();
                app.reduce(AppAction::SetTaskState(TaskState::Exiting));
                break 'run Ok(());
            }
        };
        let poll_cpu_spent = match (poll_cpu_started, thread_cpu_time()) {
            (Some(started), Some(ended)) => ended.saturating_sub(started),
            _ => Duration::ZERO,
        };
        if terminal_input_health.record_poll(
            has_terminal_event,
            poll_started_at.elapsed(),
            poll_cpu_spent,
        ) {
            tracing::warn!(
                fast_empty_polls = terminal_input_health.fast_empty_polls,
                busy_empty_polls = terminal_input_health.busy_empty_polls,
                poll_timeout_ms = EVENT_POLL_TIMEOUT.as_millis() as u64,
                "terminal input appears disconnected; exiting TUI"
            );
            tasks.abort();
            app.reduce(AppAction::SetTaskState(TaskState::Exiting));
            break 'run Ok(());
        }

        if has_terminal_event {
            let terminal_event = match event::read() {
                Ok(event) => Some(event),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => None,
                Err(err) => {
                    tracing::warn!(error = %err, "terminal event read failed; exiting TUI");
                    tasks.abort();
                    app.reduce(AppAction::SetTaskState(TaskState::Exiting));
                    break 'run Ok(());
                }
            };
            // Pointer motion is discarded wholesale (see
            // `terminal_event_is_pointer_motion`): it must not request a
            // redraw, clear the Ctrl+C prompt, or pay the terminal-size ioctl.
            let terminal_event =
                terminal_event.filter(|event| !terminal_event_is_pointer_motion(event));
            if let Some(terminal_event) = terminal_event {
                redraw_requested = true;
                if terminal_event_requests_immediate_redraw(&terminal_event) {
                    let draw_started = Instant::now();
                    draw_tui_frame(&mut terminal, &mut app)?;
                    let frame_draw = draw_started.elapsed();
                    redraw_requested = false;
                    next_redraw_at = Instant::now()
                        + redraw_interval(
                            any_task_active(
                                &app,
                                &tasks,
                                &subagents,
                                &background_tasks,
                                &terminals,
                            )
                            .await,
                        );
                    if frame_draw >= SLOW_FRAME_WARN_THRESHOLD {
                        tracing::warn!(
                            draw_ms = frame_draw.as_millis() as u64,
                            task_state = ?app.task_state,
                            "slow immediate resize redraw"
                        );
                    }
                }
                match terminal_event {
                    Event::Key(key) => {
                        let intent = map_key(key, &app);
                        if clears_ctrl_c_prompt(&intent) {
                            clear_ctrl_c_prompt(&mut app, &mut ctrl_c_prompt);
                        }
                        match intent {
                            KeyIntent::Action(action) => {
                                // The Alt+S keybind opens `/subagents` from the cached
                                // snapshot; refresh it first so the modal opens to
                                // current data even though live refresh (above) is
                                // gated on the modal being open.
                                if matches!(action, AppAction::OpenSubtaskList) {
                                    app.reduce(AppAction::RefreshSubtaskList {
                                        subtasks: subagents.list(),
                                    });
                                }
                                let mut persistence = PersistenceCommandState {
                                    current_session_id: &mut current_session_id,
                                    signatures: &mut persisted_signatures,
                                };
                                let result = handle_runtime_action(
                                    action,
                                    &mut app,
                                    &mut tasks,
                                    runtime_action_deps!(),
                                    &mut persistence,
                                )
                                .await;
                                if let RuntimeActionResult::Unhandled(action) = result {
                                    apply_action_with_task_side_effects(*action, &mut app, &tasks);
                                }
                            }
                            KeyIntent::Quit => {
                                tasks.abort();
                                clear_ctrl_c_prompt(&mut app, &mut ctrl_c_prompt);
                                app.reduce(AppAction::SetTaskState(TaskState::Exiting));
                                break 'run Ok(());
                            }
                            KeyIntent::CancelOrQuit => {
                                let now = Instant::now();
                                let has_running_subagents = subagents
                                    .list_detached()
                                    .iter()
                                    .any(|subagent| !subagent.status.is_finished());
                                match next_ctrl_c_action(
                                    app.task_state,
                                    has_running_subagents,
                                    ctrl_c_prompt,
                                    now,
                                ) {
                                    CtrlCAction::ConfirmExit => {
                                        tracing::info!(
                                            task_state = ?app.task_state,
                                            "ctrl+c: exit confirmed; breaking run loop"
                                        );
                                        tasks.abort();
                                        clear_ctrl_c_prompt(&mut app, &mut ctrl_c_prompt);
                                        app.clear_run_timer();
                                        app.reduce(AppAction::SetTaskState(TaskState::Exiting));
                                        break 'run Ok(());
                                    }
                                    CtrlCAction::CancelRunAndArm => {
                                        let queued_ids = app.queued_input_ids();
                                        if let Err(err) = tasks.cancel_agent_messages(&queued_ids) {
                                            app.reduce(AppAction::Agent(UiEvent::Error(err)));
                                        }
                                        app.reduce(AppAction::DropQueuedInput);
                                        tasks.cancel();
                                        tracing::info!(
                                            "ctrl+c: cancelling running task; exit armed"
                                        );
                                        app.reduce(AppAction::SetTaskState(TaskState::Cancelling));
                                        set_ctrl_c_prompt(
                                            &mut app,
                                            &mut ctrl_c_prompt,
                                            CtrlCExitPromptKind::Cancelling,
                                            now,
                                        );
                                    }
                                    CtrlCAction::CancelSubagentsAndArm => {
                                        agent.lock().await.cancel_running_subagents();
                                        tracing::info!(
                                            "ctrl+c: cancelling background subagents; exit armed"
                                        );
                                        set_ctrl_c_prompt(
                                            &mut app,
                                            &mut ctrl_c_prompt,
                                            CtrlCExitPromptKind::Cancelling,
                                            now,
                                        );
                                    }
                                    CtrlCAction::CancelCommandAndArm => {
                                        tasks.abort();
                                        // Invalidate any command spawned on a raw
                                        // `tokio::spawn` (auth/model/local-model):
                                        // `abort` doesn't reach those, so bump the
                                        // generation to drop their late, stale
                                        // `CommandFinished` instead of letting it
                                        // flip provider/model after we go idle.
                                        app.invalidate_pending_commands();
                                        app.clear_run_timer();
                                        app.reduce(AppAction::SetTaskState(TaskState::Idle));
                                        set_ctrl_c_prompt(
                                            &mut app,
                                            &mut ctrl_c_prompt,
                                            CtrlCExitPromptKind::Exit,
                                            now,
                                        );
                                    }
                                    CtrlCAction::ArmExit => {
                                        tracing::info!(
                                            task_state = ?app.task_state,
                                            "ctrl+c: armed exit prompt"
                                        );
                                        set_ctrl_c_prompt(
                                            &mut app,
                                            &mut ctrl_c_prompt,
                                            CtrlCExitPromptKind::Exit,
                                            now,
                                        );
                                    }
                                }
                            }
                            intent @ (KeyIntent::Submit | KeyIntent::SubmitReplacement(_)) => {
                                if let KeyIntent::SubmitReplacement(replacement) = &intent {
                                    app.reduce(AppAction::CompleteInputTo(replacement.clone()));
                                }
                                // Snapshot the composer once: `input` is the
                                // placeholder display text used for slash-command
                                // detection and the transcript, while
                                // `submission.input` carries the expanded model
                                // payload (chip text spliced in, images attached).
                                let submission = app.composer.submission();
                                let input = submission.display_text.trim().to_string();
                                if input.is_empty() {
                                    continue;
                                }
                                if app.handle_copy_command(&input) {
                                    app.reduce(AppAction::SubmitCommandInput(input.clone()));
                                    continue;
                                }
                                if open_tasks_command_if_exact(
                                    &input,
                                    &mut app,
                                    &background_tasks,
                                    &terminals,
                                )
                                .await
                                {
                                    continue;
                                }
                                if matches!(input.trim(), "/subagents" | "/subtasks") {
                                    app.reduce(AppAction::SubmitCommandInput(
                                        input.trim().to_string(),
                                    ));
                                    app.reduce(AppAction::RefreshSubtaskList {
                                        subtasks: subagents.list(),
                                    });
                                    app.reduce(AppAction::OpenSubtaskList);
                                    continue;
                                }
                                if input.trim() == "/peers" {
                                    app.reduce(AppAction::SubmitCommandInput(
                                        input.trim().to_string(),
                                    ));
                                    let peers = peer_bus.overview().await.unwrap_or_default();
                                    app.reduce(AppAction::OpenPeerList { peers });
                                    continue;
                                }
                                if input.trim() == "/refresh" {
                                    app.reduce(AppAction::SubmitCommandInput(
                                        input.trim().to_string(),
                                    ));
                                    let mut persistence = PersistenceCommandState {
                                        current_session_id: &mut current_session_id,
                                        signatures: &mut persisted_signatures,
                                    };
                                    handle_runtime_action(
                                        AppAction::StartRefresh,
                                        &mut app,
                                        &mut tasks,
                                        runtime_action_deps!(),
                                        &mut persistence,
                                    )
                                    .await;
                                    continue;
                                }
                                // `/usage` works in any task state, like `/peers`:
                                // the dashboard reads only persisted aggregates.
                                if input.trim() == "/usage" {
                                    app.reduce(AppAction::SubmitCommandInput(
                                        input.trim().to_string(),
                                    ));
                                    match storage.load_usage_dashboard().await {
                                        Ok(dashboard) => {
                                            app.reduce(AppAction::OpenUsageDashboard {
                                                dashboard: Box::new(dashboard),
                                            });
                                        }
                                        Err(err) => push_command_message(
                                            &mut app,
                                            CommandOutputKind::Error,
                                            &format!("Failed to load usage analytics: {err:#}"),
                                        ),
                                    }
                                    continue;
                                }
                                // `/bonsai` is pure decoration and works in any task
                                // state: reseed and regrow the welcome/sidebar trees.
                                // Deliberately no transcript echo — an echoed command
                                // would end the empty-transcript welcome screen the
                                // hero tree lives on.
                                if input.trim() == "/bonsai" {
                                    app.reduce(AppAction::SubmitCommandInput(
                                        input.trim().to_string(),
                                    ));
                                    app.reduce(AppAction::ReplantBonsai);
                                    continue;
                                }
                                // `/permissions` works in any task state and needs the
                                // shared manager, so handle it here rather than in the
                                // running/idle slash-command paths.
                                if matches!(input.split_whitespace().next(), Some("/permissions")) {
                                    app.reduce(AppAction::ScrollBottom);
                                    app.reduce(AppAction::SubmitCommandInput(input.clone()));
                                    apply_permissions_command(
                                        &input,
                                        &mut app,
                                        &permissions,
                                        &domain_permissions,
                                        &storage,
                                        Some(current_session_id),
                                    )
                                    .await;
                                    continue;
                                }
                                // `/agents` works in any task state: the browser reads the
                                // shared custom-agent registry and builtin-subagent settings
                                // handles — never the agent lock, which a running turn holds
                                // for its entire duration (locking here froze the whole TUI
                                // until the turn ended).
                                if input.trim() == "/agents" {
                                    app.reduce(AppAction::SubmitCommandInput(
                                        input.trim().to_string(),
                                    ));
                                    let rows =
                                        crate::tui::agent_composer::browser_rows_with_settings(
                                            &crate::resource::agent::snapshot(&app.custom_agents),
                                            &app.builtin_subagents.snapshot(),
                                        );
                                    app.reduce(AppAction::OpenModal(ModalKind::AgentBrowser {
                                        rows,
                                        cursor: 0,
                                    }));
                                    continue;
                                }
                                // `/skills` works in any task state: rows build from the
                                // shared registry handle plus the loaded-skills mirror —
                                // never the agent lock (see `/agents` above).
                                if input.trim() == "/skills" {
                                    app.reduce(AppAction::SubmitCommandInput(
                                        input.trim().to_string(),
                                    ));
                                    let rows = crate::tui::skill_manager::skill_rows(
                                        &crate::resource::skill::snapshot(&app.skills),
                                        &app.loaded_skills,
                                    );
                                    app.reduce(AppAction::OpenModal(ModalKind::SkillManager {
                                        rows,
                                        cursor: 0,
                                    }));
                                    continue;
                                }
                                // `/providers` works in any task state: the provider
                                // manager reads from the model catalog and registry
                                // without touching the conversation.
                                if matches!(input.split_whitespace().next(), Some("/providers")) {
                                    app.reduce(AppAction::ScrollBottom);
                                    app.reduce(AppAction::SubmitCommandInput(input.clone()));
                                    let providers_args =
                                        input.strip_prefix("/providers").unwrap_or("").trim();
                                    crate::tui::runtime_actions::handle_providers_command(
                                        providers_args,
                                        &mut app,
                                        runtime_action_deps!(),
                                    )
                                    .await;
                                    continue;
                                }
                                if matches!(app.task_state, TaskState::Running) {
                                    if input.starts_with('/') {
                                        let mut persistence = PersistenceCommandState {
                                            current_session_id: &mut current_session_id,
                                            signatures: &mut persisted_signatures,
                                        };
                                        if handle_running_slash_command(
                                            &input,
                                            &mut app,
                                            PersistenceCommandDeps {
                                                storage: &storage,
                                                agent: agent.clone(),
                                                memory: memory.clone(),
                                                session_store: session_store.clone(),
                                                registry: registry.clone(),
                                                model_catalog: model_catalog.clone(),
                                                todo_store: todo_store.clone(),
                                                plan_store: plan_store.clone(),
                                                project_root: &session_project_root,
                                                active_session_id: active_session_id.clone(),
                                            },
                                            &yolo_mode,
                                            &mut persistence,
                                        )
                                        .await
                                        {
                                            continue;
                                        }
                                    }
                                    queue_running_submit(
                                        &mut app,
                                        &tasks,
                                        intent,
                                        &registry,
                                        Some(&model_catalog),
                                    );
                                    continue;
                                }
                                if matches!(
                                    app.task_state,
                                    TaskState::Command | TaskState::Cancelling | TaskState::Exiting
                                ) {
                                    continue;
                                }
                                if let Some(command) = idle_slash_command(&input) {
                                    // Every inline command echoes into the
                                    // transcript the same way; the arms below do
                                    // only their distinctive work.
                                    app.reduce(AppAction::ScrollBottom);
                                    app.reduce(AppAction::SubmitCommandInput(input.clone()));
                                    match command {
                                        IdleSlashCommand::Theme(arg) => {
                                            if arg.is_empty() {
                                                open_theme_picker(&mut app);
                                            } else {
                                                apply_theme_command(
                                                    arg,
                                                    &mut app,
                                                    &project_root,
                                                    session_store.clone(),
                                                )
                                                .await;
                                            }
                                        }
                                        IdleSlashCommand::Start => {
                                            open_start_plan_choice(&mut app, &plan_store).await;
                                        }
                                        IdleSlashCommand::Continue => {
                                            // Resume from the first pending phase, skipping the
                                            // StartPlanChoice modal.
                                            implement_plan_with_context(
                                                &mut app,
                                                &mut tasks,
                                                agent.clone(),
                                                sink.clone(),
                                                todo_store.clone(),
                                                plan_store.clone(),
                                                crate::agent::PlanContextMode::Keep,
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Test => {
                                            run_test_profile(
                                                &mut app,
                                                &mut tasks,
                                                agent.clone(),
                                                sink.clone(),
                                                &project_root,
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Build => {
                                            run_build_profile(
                                                &mut app,
                                                &mut tasks,
                                                agent.clone(),
                                                sink.clone(),
                                                &project_root,
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Retry => {
                                            retry_last_turn(
                                                &input,
                                                &mut app,
                                                &mut tasks,
                                                RetryCommandDeps {
                                                    agent: agent.clone(),
                                                    sink: sink.clone(),
                                                    session_store: session_store.clone(),
                                                    registry: registry.clone(),
                                                    model_catalog: model_catalog.clone(),
                                                    storage: &storage,
                                                    session_id: current_session_id,
                                                },
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Commit => {
                                            commit_changes(
                                                &mut app,
                                                &mut tasks,
                                                agent.clone(),
                                                sink.clone(),
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::PullRequest => {
                                            create_pull_request(
                                                &mut app,
                                                &mut tasks,
                                                agent.clone(),
                                                sink.clone(),
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Review => {
                                            if !app.task_state.is_busy() {
                                                app.reduce(AppAction::OpenModal(
                                                    ModalKind::ReviewScopePicker { cursor: 0 },
                                                ));
                                            }
                                        }
                                        IdleSlashCommand::SecurityReview => {
                                            security_review_changes(
                                                &mut app,
                                                &mut tasks,
                                                agent.clone(),
                                                sink.clone(),
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Mode => {
                                            // Rows are seeded by the reducer from
                                            // current app state; pass an empty
                                            // placeholder so OpenModal can rebind
                                            // them.
                                            app.reduce(AppAction::OpenModal(
                                                ModalKind::ModePicker {
                                                    rows: Vec::new(),
                                                    cursor: 0,
                                                },
                                            ));
                                        }
                                        IdleSlashCommand::Settings => {
                                            // SMOL lives on the agent, so seed here
                                            // (with the lock) rather than in the
                                            // reducer.
                                            let smol_profile =
                                                { agent.lock().await.smol_profile() };
                                            let rows = crate::tui::settings::seed_settings_rows(
                                                &app,
                                                smol_profile,
                                            );
                                            let cursor =
                                                crate::tui::settings::first_selectable(&rows);
                                            app.reduce(AppAction::OpenModal(ModalKind::Settings {
                                                rows,
                                                cursor,
                                            }));
                                        }
                                        IdleSlashCommand::Wizard => {
                                            match local_model_wizard_state_for_input(
                                                &input,
                                                &model_catalog,
                                                storage.home_dir(),
                                                app.credential_persistence,
                                            ) {
                                                Ok(state) => {
                                                    app.reduce(AppAction::OpenModal(
                                                        ModalKind::LocalModelWizard {
                                                            state: Box::new(state),
                                                        },
                                                    ));
                                                }
                                                Err(message) => {
                                                    push_command_message(
                                                        &mut app,
                                                        CommandOutputKind::Error,
                                                        &message,
                                                    );
                                                }
                                            }
                                        }
                                        IdleSlashCommand::Providers(args) => {
                                            crate::tui::runtime_actions::handle_providers_command(
                                                args,
                                                &mut app,
                                                runtime_action_deps!(),
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Skills => {
                                            let rows = crate::tui::skill_manager::skill_rows(
                                                &crate::resource::skill::snapshot(&app.skills),
                                                &app.loaded_skills,
                                            );
                                            app.reduce(AppAction::OpenModal(
                                                ModalKind::SkillManager { rows, cursor: 0 },
                                            ));
                                        }
                                        IdleSlashCommand::AgentComposer => {
                                            app.reduce(AppAction::OpenModal(
                                                ModalKind::AgentComposer {
                                                    state: Box::new(
                                                        crate::tui::agent_composer::AgentComposerState::new(),
                                                    ),
                                                },
                                            ));
                                        }
                                        IdleSlashCommand::AgentBrowser => {
                                            let rows =
                                                crate::tui::agent_composer::browser_rows_with_settings(
                                                    &crate::resource::agent::snapshot(&app.custom_agents),
                                                    &app.builtin_subagents.snapshot(),
                                                );
                                            app.reduce(AppAction::OpenModal(
                                                ModalKind::AgentBrowser { rows, cursor: 0 },
                                            ));
                                        }
                                        IdleSlashCommand::Model => {
                                            open_cached_model_picker(
                                                &mut app,
                                                session_store.clone(),
                                                registry.clone(),
                                                model_catalog.clone(),
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Ctx => {
                                            open_context_modal_with_preview(
                                                &mut app,
                                                agent.clone(),
                                                tasks.is_busy(),
                                                false,
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Episodes => {
                                            if app.latest_context_report.is_some() {
                                                app.reduce(AppAction::OpenEpisodesModal);
                                            } else {
                                                push_command_message(
                                                    &mut app,
                                                    CommandOutputKind::Status,
                                                    "Context report is not available yet.",
                                                );
                                            }
                                        }
                                        IdleSlashCommand::Autonomy => {
                                            apply_autonomy_command(
                                                &input, &mut app, &yolo_mode, &storage,
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::SelfReview => {
                                            apply_self_review_command(
                                                &input,
                                                &mut app,
                                                agent.clone(),
                                                &session_store,
                                                &registry,
                                                &model_catalog,
                                                &storage,
                                                true,
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Smol => {
                                            apply_smol_command(
                                                &input,
                                                &mut app,
                                                agent.clone(),
                                                &storage,
                                                true,
                                            )
                                            .await;
                                        }
                                        IdleSlashCommand::Serenity => {
                                            apply_serenity_command(&input, &mut app, &storage)
                                                .await;
                                        }
                                        IdleSlashCommand::Sandbox => {
                                            apply_sandbox_command(&input, &mut app, &storage).await;
                                        }
                                        IdleSlashCommand::Persistence(command) => {
                                            let mut persistence = PersistenceCommandState {
                                                current_session_id: &mut current_session_id,
                                                signatures: &mut persisted_signatures,
                                            };
                                            if let Err(err) = apply_persistence_command(
                                                command,
                                                &mut app,
                                                PersistenceCommandDeps {
                                                    storage: &storage,
                                                    agent: agent.clone(),
                                                    memory: memory.clone(),
                                                    session_store: session_store.clone(),
                                                    registry: registry.clone(),
                                                    model_catalog: model_catalog.clone(),
                                                    todo_store: todo_store.clone(),
                                                    plan_store: plan_store.clone(),
                                                    project_root: &session_project_root,
                                                    active_session_id: active_session_id.clone(),
                                                },
                                                &mut persistence,
                                            )
                                            .await
                                            {
                                                push_command_message(
                                                    &mut app,
                                                    CommandOutputKind::Error,
                                                    &format!("{err:#}"),
                                                );
                                            }
                                        }
                                    }
                                    continue;
                                }

                                if input.starts_with('/') {
                                    app.reduce(AppAction::ScrollBottom);
                                    app.reduce(AppAction::SubmitCommandInput(input.clone()));
                                    app.reduce(AppAction::SetTaskState(TaskState::Command));
                                    let command_deps = CommandTaskDeps::new(
                                        agent.clone(),
                                        session_store.clone(),
                                        project_root.clone(),
                                        registry.clone(),
                                        model_catalog.clone(),
                                        Some(storage.clone()),
                                        app.credential_persistence,
                                    );
                                    if let Err(err) = tasks.start_command(
                                        input,
                                        app.transcript.to_vec(),
                                        app.command_generation(),
                                        command_deps,
                                    ) {
                                        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(
                                            err,
                                        )));
                                    }
                                } else {
                                    // Vision gate: an attached image on a model
                                    // that can't see images aborts the submit and
                                    // leaves the composer intact rather than
                                    // silently dropping the image.
                                    if submission.input.has_images()
                                        && !crate::model_resolution::model_supports_vision(
                                            &registry,
                                            Some(&model_catalog),
                                            &app.provider,
                                            &app.model,
                                        )
                                    {
                                        push_command_message(
                                            &mut app,
                                            CommandOutputKind::Error,
                                            "This model can't see images — remove the [Image N] chip or switch models.",
                                        );
                                        continue;
                                    }
                                    // Plan view routes prompts to the planning agent;
                                    // the agent view talks to the coding agent.
                                    let persona = app.active_persona.clone();
                                    app.reduce(AppAction::SetTaskState(TaskState::Running));
                                    app.mark_run_started(std::time::Instant::now());
                                    app.reduce(AppAction::ScrollBottom);
                                    app.reduce(AppAction::SubmitInput(input.clone()));
                                    let phase = app.active_mode().run_status_label();
                                    app.reduce(AppAction::Agent(UiEvent::Thinking(format!(
                                        "{phase}: {}",
                                        summarize_input(&input)
                                    ))));
                                    maybe_seed_session_title(&mut app, &storage, &input).await;
                                    sync_agent_self_review_mode(app.self_review_mode, &agent).await;
                                    if let Some(report) = repo_map.apply_before_turn(&agent).await {
                                        app.latest_context_report = Some(report);
                                    }
                                    if let Err(err) = tasks.start_agent_run(
                                        agent.clone(),
                                        submission.input,
                                        sink.clone(),
                                        persona,
                                    ) {
                                        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(
                                            err,
                                        )));
                                    }
                                }
                            }
                            KeyIntent::Insert(ch) => app.reduce(AppAction::InputChar(ch)),
                            KeyIntent::Noop => {}
                        }
                    }
                    Event::Mouse(mouse) => {
                        clear_ctrl_c_prompt(&mut app, &mut ctrl_c_prompt);
                        let area = terminal.terminal_mut().size()?.into();
                        if let Some(action) = map_mouse(mouse, &app, area) {
                            if matches!(action, AppAction::OpenContextModal) {
                                open_context_modal_with_preview(
                                    &mut app,
                                    agent.clone(),
                                    tasks.is_busy(),
                                    true,
                                )
                                .await;
                            } else {
                                apply_action_with_task_side_effects(action, &mut app, &tasks);
                            }
                        }
                    }
                    Event::Paste(text) => {
                        clear_ctrl_c_prompt(&mut app, &mut ctrl_c_prompt);
                        if matches!(app.modal, Some(ModalKind::ApiKeyPrompt { .. })) {
                            app.reduce(AppAction::ApiKeyInputPaste(text));
                        } else if matches!(app.modal, Some(ModalKind::LocalModelWizard { .. })) {
                            app.reduce(AppAction::LocalModelWizardPaste(text));
                        } else if matches!(app.modal, Some(ModalKind::AgentComposer { .. })) {
                            app.reduce(AppAction::AgentComposerPaste(text));
                        } else if matches!(app.focus, Focus::Input) && text.trim().is_empty() {
                            // Some terminals deliver an empty bracketed paste when
                            // the clipboard holds only an image; recover it via the
                            // clipboard-image path rather than inserting nothing.
                            crate::tui::runtime_actions::paste_from_clipboard(
                                &mut app,
                                &registry,
                                Some(&model_catalog),
                            );
                        } else {
                            app.reduce(AppAction::PasteText(text));
                        }
                    }
                    _ => {}
                }
            }
        }

        if tasks.poll_finished().await.is_some() {
            redraw_requested = true;
            if matches!(app.task_state, TaskState::Cancelling) {
                app.reduce(AppAction::Agent(UiEvent::Interrupted));
            }
            app.mark_run_finished(Instant::now());
            let started_deferred = start_next_deferred_command_if_idle(
                &mut app,
                &mut tasks,
                agent.clone(),
                session_store.clone(),
                &project_root,
                DeferredCommandDeps {
                    registry: registry.clone(),
                    model_catalog: model_catalog.clone(),
                    storage: Some(&storage),
                },
            )
            .await;
            redraw_requested |= started_deferred;
            if !started_deferred {
                // A confirmed plan-mode switch continues the just-finished turn
                // under the new persona; it takes priority over phase-advance
                // and queued runs for the idle slot.
                let switched = maybe_apply_persona_switch(
                    &mut app,
                    &mut tasks,
                    agent.clone(),
                    sink.clone(),
                    &mut repo_map,
                )
                .await;
                redraw_requested |= switched;
                // A finished phased-plan run auto-advances to the next phase. When it
                // starts the next phase, skip the queued-run start so the two don't
                // race for the idle slot.
                let advanced = !switched
                    && maybe_advance_plan_phase(
                        &mut app,
                        &mut tasks,
                        agent.clone(),
                        sink.clone(),
                        todo_store.clone(),
                        plan_store.clone(),
                        &mut repo_map,
                    )
                    .await;
                redraw_requested |= advanced;
                if !switched && !advanced {
                    redraw_requested |= start_pending_queued_run_if_idle(
                        &mut app,
                        &mut tasks,
                        agent.clone(),
                        sink.clone(),
                        &mut repo_map,
                        &registry,
                        &model_catalog,
                    )
                    .await;
                }
            }
            // A persona selected while the run was in flight was deliberately
            // not applied mid-run (the busy guard skipped it). Apply it now
            // that the run ended — completed, failed, or interrupted alike.
            // When one of the dispatchers above already started a follow-up
            // run, its busy guard makes this a no-op (each dispatch sets its
            // own persona anyway).
            sync_agent_mode_and_refresh(&mut app, agent.clone()).await;
        }

        // After all event processing for the frame, a view change (Tab, `1`,
        // `2`, mouse, implement_plan, review_changes) means the selected agent
        // persona changed, so the tool-schema count in the persisted context
        // report is stale. Regenerate it before the next draw.
        //
        // A persona change never cancels an in-flight run: the running turn
        // captured its persona at dispatch and keeps it — the user may just be
        // peeking at the plan canvas mid-implementation, and the review agent's
        // first finding auto-opens that canvas under the reviewer's feet
        // (regression: session 7, the finding's own auto-open cancelled the
        // review 228µs after it was recorded). While busy this sync is a no-op
        // (its internal guard); the post-completion sync in the poll_finished
        // block applies the selection once the run ends, and every dispatch
        // path sets its persona explicitly, so the next user-sent run always
        // starts under the newly selected persona.
        if app.active_persona != last_active_persona {
            last_active_persona = app.active_persona.clone();
            sync_agent_mode_and_refresh(&mut app, agent.clone()).await;
            redraw_requested = true;
        }
    };

    shutdown_tui(
        terminal,
        app,
        agent,
        storage,
        current_session_id,
        background_tasks,
        terminals,
        persisted_signatures,
        peer_bus,
        pending_snapshot_flush,
    )
    .await?;
    result
}

fn prepare_termination_shutdown(app: &mut AppState) {
    app.current_session_status = SessionStatus::Interrupted;
    app.current_terminal_reason = None;
    app.reduce(AppAction::SetTaskState(TaskState::Exiting));
}

/// Everything that runs once the frame loop breaks: log shutdown stats,
/// snapshot the final context report, stop background tasks, mark the
/// session complete, restore the terminal, flush final persistence, and
/// drop/close every long-lived handle in an order that keeps each step
/// attributable in the trace. Takes ownership of the handles the loop no
/// longer needs, so this is genuinely their last use.
///
/// `terminal.restore()?` can return early, in which case the steps below it
/// (final persistence, drops, storage close) never run — matching `run`'s
/// prior behavior when this was inline: a restore failure always skipped the
/// rest of teardown rather than trying to limp through it.
#[allow(clippy::too_many_arguments)]
async fn shutdown_tui(
    mut terminal: TerminalSession,
    mut app: AppState,
    agent: Arc<Mutex<Agent>>,
    storage: Storage,
    current_session_id: SessionId,
    background_tasks: Arc<BackgroundTaskRegistry>,
    terminals: Arc<TerminalRegistry>,
    mut persisted_signatures: PersistedSnapshotSignatures,
    peer_bus: Arc<crate::peer::PeerBus>,
    pending_snapshot_flush: Option<tokio::task::JoinHandle<PersistenceFlushResult>>,
) -> Result<()> {
    let shutdown_started_at = Instant::now();
    let app_stats = shutdown_app_stats(&app);
    tracing::info!(
        session_id = %current_session_id,
        transcript_items = app_stats.transcript_items,
        transcript_text_bytes = app_stats.transcript_text_bytes,
        tool_activities = app_stats.tool_activities,
        tool_text_bytes = app_stats.tool_text_bytes,
        diff_lines = app_stats.diff_lines,
        diff_text_bytes = app_stats.diff_text_bytes,
        queued_inputs = app_stats.queued_inputs,
        plan_sections = app_stats.plan_sections,
        plan_tasks = app_stats.plan_tasks,
        todos = app_stats.todos,
        "TUI shutdown started"
    );

    // SessionEnd: capped independently of any hook's own configured
    // timeout — shutdown must never hang on a misbehaving hook.
    const SESSION_END_HOOK_TIMEOUT: Duration = Duration::from_secs(5);
    let hook_started_at = Instant::now();
    let session_end_hooks = agent.lock().await.hooks().clone();
    match tokio::time::timeout(
        SESSION_END_HOOK_TIMEOUT,
        session_end_hooks.fire(
            crate::hooks::HookEvent::SessionEnd,
            crate::hooks::HookContext::default(),
        ),
    )
    .await
    {
        Ok(outcome) => {
            for warning in &outcome.warnings {
                tracing::warn!(session_id = %current_session_id, %warning, "SessionEnd hook warning");
            }
        }
        Err(_) => {
            tracing::warn!(
                session_id = %current_session_id,
                elapsed_ms = elapsed_ms(hook_started_at),
                "SessionEnd hooks did not finish within the shutdown cap"
            );
        }
    }
    tracing::info!(
        session_id = %current_session_id,
        elapsed_ms = elapsed_ms(hook_started_at),
        "TUI SessionEnd hooks completed"
    );

    let context_started_at = Instant::now();
    let context_report = app
        .latest_context_report
        .clone()
        .or_else(|| agent.try_lock().ok().map(|agent| agent.context_report()));
    let session_end_summary = context_report.as_ref().and_then(|report| {
        session_end_usage_summary(current_session_id, &app.provider, &app.model, report)
    });
    tracing::info!(
        session_id = %current_session_id,
        elapsed_ms = elapsed_ms(context_started_at),
        context_entries = context_report.as_ref().map_or(0, |report| report.entries.len()),
        prompt_estimate_tokens = context_report.as_ref().map_or(0, |report| report.prompt_estimate_tokens),
        summary_sources = context_report.as_ref().map_or(0, |report| report.summary_sources.len()),
        compaction_events = context_report.as_ref().map_or(0, |report| report.compaction_events.len()),
        usage_turns = context_report.as_ref().map_or(0, |report| report.usage_turns.len()),
        "TUI shutdown context snapshot completed"
    );

    let background_shutdown_started_at = Instant::now();
    let stopped_background_tasks = background_tasks
        .stop_all_running(BACKGROUND_TASK_SHUTDOWN_TIMEOUT)
        .await;
    for task in &stopped_background_tasks {
        let _ = apply_background_task_snapshot(&mut app, task);
    }
    let stopped_terminals = terminals
        .stop_all_running(BACKGROUND_TASK_SHUTDOWN_TIMEOUT)
        .await;
    for terminal in &stopped_terminals {
        let _ = apply_terminal_snapshot(&mut app, terminal);
    }
    if (!stopped_background_tasks.is_empty() || !stopped_terminals.is_empty())
        && app.is_task_list_open()
    {
        refresh_background_task_list(&mut app, &background_tasks, &terminals).await;
    }
    let still_running = stopped_background_tasks
        .iter()
        .filter(|task| !task.status.is_finished())
        .count();
    // Logged unconditionally (even with nothing to stop): this phase carries a
    // 2s timeout, so it must never be a silent gap when the shutdown drags.
    tracing::info!(
        session_id = %current_session_id,
        stopped = stopped_background_tasks.len().saturating_sub(still_running)
            + stopped_terminals
                .iter()
                .filter(|terminal| terminal.status.is_finished())
                .count(),
        still_running = still_running
            + stopped_terminals
                .iter()
                .filter(|terminal| !terminal.status.is_finished())
                .count(),
        elapsed_ms = elapsed_ms(background_shutdown_started_at),
        "TUI background task shutdown completed"
    );

    // A quitting session can never fire its wake subscriptions again — notify
    // waiters now so none stays parked on a session that cleanly exited (the
    // requester-side sweep would catch it anyway, but only after the liveness
    // threshold).
    if let Err(err) = peer_bus.fire_own_wake_subscriptions().await {
        tracing::warn!(
            session_id = %current_session_id,
            error = %format!("{err:#}"),
            "failed to fire peer wake subscriptions on quit"
        );
    }

    // Record the clean exit *first* — it's a single cheap row update, and it
    // must never be starved by the (potentially slow) snapshot persistence
    // below or skipped by an early return from `terminal.restore()?`. Ordered
    // after the snapshot flush inside one shared 1s budget, a slow flush leaves
    // the row `Active`, which the next launch promotes to `Interrupted` —
    // mis-reporting a genuine clean quit as a crash.
    if let Err(err) = storage
        .mark_session_termination(
            current_session_id,
            terminal_session_status(&app.current_session_status),
            app.current_terminal_reason,
        )
        .await
    {
        tracing::warn!(
            session_id = %current_session_id,
            error = %err,
            "failed to mark persisted session complete"
        );
    }

    let terminal_started_at = Instant::now();
    terminal.restore()?;
    tracing::info!(
        session_id = %current_session_id,
        elapsed_ms = elapsed_ms(terminal_started_at),
        "TUI terminal restore completed"
    );
    if let Some(summary) = session_end_summary {
        println!("{summary}");
        println!("{}", format_resume_hint(current_session_id));
    }

    if let Some(mut pending) = pending_snapshot_flush {
        match tokio::time::timeout(FINAL_PERSISTENCE_TIMEOUT, &mut pending).await {
            Ok(Ok(flush)) => apply_persistence_flush_result(
                flush,
                current_session_id,
                &mut app,
                &agent,
                &mut persisted_signatures,
            ),
            Ok(Err(error)) => tracing::warn!(
                session_id = %current_session_id,
                error = %error,
                "periodic snapshot task failed during shutdown"
            ),
            Err(_) => {
                pending.abort();
                let _ = pending.await;
                tracing::warn!(
                    session_id = %current_session_id,
                    "timed out waiting for periodic snapshot during shutdown"
                );
            }
        }
    }

    let persistence_started_at = Instant::now();
    let final_persistence = persist_changed_snapshots(
        &storage,
        current_session_id,
        &mut app,
        agent.clone(),
        &mut persisted_signatures,
    );
    if tokio::time::timeout(FINAL_PERSISTENCE_TIMEOUT, final_persistence)
        .await
        .is_err()
    {
        tracing::warn!(
            session_id = %current_session_id,
            elapsed_ms = elapsed_ms(persistence_started_at),
            "timed out while persisting final session state"
        );
    } else {
        tracing::info!(
            session_id = %current_session_id,
            elapsed_ms = elapsed_ms(persistence_started_at),
            "TUI final persistence completed"
        );
    }

    let app_drop_started_at = Instant::now();
    drop(app);
    tracing::info!(
        session_id = %current_session_id,
        elapsed_ms = elapsed_ms(app_drop_started_at),
        total_elapsed_ms = elapsed_ms(shutdown_started_at),
        "TUI app state dropped"
    );
    let agent_refs_before_drop = Arc::strong_count(&agent);
    let agent_drop_started_at = Instant::now();
    drop(agent);
    tracing::info!(
        session_id = %current_session_id,
        strong_refs_before_drop = agent_refs_before_drop,
        elapsed_ms = elapsed_ms(agent_drop_started_at),
        total_elapsed_ms = elapsed_ms(shutdown_started_at),
        "TUI agent handle dropped"
    );

    // Close the SQLite pool explicitly, on a measured await, before returning.
    // Left to an implicit `Drop`, the last-connection WAL checkpoint (seconds on
    // a large WAL) runs untimed after the runtime is gone and stalls process
    // exit invisibly. Here it is bounded, logged, and attributable.
    let storage_close_started_at = Instant::now();
    storage.close().await;
    tracing::info!(
        session_id = %current_session_id,
        elapsed_ms = elapsed_ms(storage_close_started_at),
        total_elapsed_ms = elapsed_ms(shutdown_started_at),
        "TUI storage pool closed"
    );
    Ok(())
}

fn elapsed_ms(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis() as u64
}

#[derive(Debug, Default)]
struct ShutdownAppStats {
    transcript_items: usize,
    transcript_text_bytes: usize,
    tool_activities: usize,
    tool_text_bytes: usize,
    diff_lines: usize,
    diff_text_bytes: usize,
    queued_inputs: usize,
    plan_sections: usize,
    plan_tasks: usize,
    todos: usize,
}

fn shutdown_app_stats(app: &AppState) -> ShutdownAppStats {
    let mut stats = ShutdownAppStats {
        transcript_items: app.transcript.len(),
        queued_inputs: app.queued_inputs.len(),
        plan_sections: app.plan.sections.len(),
        plan_tasks: app.plan.tasks.len(),
        todos: app.todo.len(),
        ..ShutdownAppStats::default()
    };
    for item in app.transcript.iter() {
        accumulate_transcript_item_stats(item, &mut stats);
    }
    stats
}

fn accumulate_transcript_item_stats(item: &TranscriptItem, stats: &mut ShutdownAppStats) {
    match item {
        TranscriptItem::UserMessage { text }
        | TranscriptItem::QueuedUserMessage { text, .. }
        | TranscriptItem::AssistantMessage { text }
        | TranscriptItem::ReasoningSummary { text }
        | TranscriptItem::CommandOutput { text, .. }
        | TranscriptItem::Edit { text }
        | TranscriptItem::TodoUpdate { text }
        | TranscriptItem::WorkLog { text }
        | TranscriptItem::PeerMessage { text, .. } => {
            stats.transcript_text_bytes = stats.transcript_text_bytes.saturating_add(text.len());
        }
        TranscriptItem::CompletionReport(report) => {
            stats.transcript_text_bytes = stats
                .transcript_text_bytes
                .saturating_add(report.render_compact().len());
        }
        TranscriptItem::ToolActivity(activity) => {
            accumulate_tool_activity_stats(activity, stats);
        }
        TranscriptItem::ExecutionGroup(group) => {
            for activity in &group.tools {
                accumulate_tool_activity_stats(activity, stats);
            }
        }
        TranscriptItem::Error { error } => {
            stats.transcript_text_bytes = stats
                .transcript_text_bytes
                .saturating_add(error.title.len())
                .saturating_add(error.detail.len());
        }
    }
}

fn accumulate_tool_activity_stats(activity: &ToolActivity, stats: &mut ShutdownAppStats) {
    stats.tool_activities = stats.tool_activities.saturating_add(1);
    stats.tool_text_bytes = stats
        .tool_text_bytes
        .saturating_add(activity.id.len())
        .saturating_add(activity.name.len())
        .saturating_add(activity.arguments.len())
        .saturating_add(activity.result.as_ref().map_or(0, String::len));
    if let Some(diff) = &activity.diff {
        stats.diff_lines = stats.diff_lines.saturating_add(
            diff.hunks
                .iter()
                .map(|hunk| hunk.lines.len())
                .sum::<usize>(),
        );
        stats.diff_text_bytes = stats.diff_text_bytes.saturating_add(
            diff.hunks
                .iter()
                .flat_map(|hunk| hunk.lines.iter())
                .map(|line| line.content.len())
                .sum::<usize>(),
        );
    }
}

/// Arm a flag that flips `true` when the process is asked to terminate via a
/// catchable signal, so the run loop can exit through the normal clean shutdown
/// instead of being killed mid-frame and leaving the session row `Active`.
///
/// Only `SIGTERM` (e.g. `kill`, IDE stop button, system logout) and `SIGHUP`
/// (controlling terminal closed) are handled. `SIGINT` is deliberately *not*
/// handled: the TUI runs in raw mode where Ctrl+C arrives as a key event and is
/// gated behind the in-app "Ctrl+C again to exit" confirmation, so trapping
/// `SIGINT` here would let a single Ctrl+C bypass that prompt on any terminal
/// that leaves `ISIG` enabled.
fn install_termination_signal_handler() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // Shared across the TERM and HUP listener tasks: when the first signal
        // arrived, so a repeat can be classified as either a same-moment
        // duplicate or an escalation. A dying pty delivers SIGHUP twice within
        // milliseconds (kernel → foreground pgrp, plus the exiting shell HUPing
        // its jobs), and hard-exiting on that burst would skip the clean
        // shutdown the first signal just requested.
        let first_signal_at = Arc::new(std::sync::Mutex::new(None::<Instant>));
        for kind in [SignalKind::terminate(), SignalKind::hangup()] {
            match signal(kind) {
                Ok(mut stream) => {
                    let flag = flag.clone();
                    let first_signal_at = first_signal_at.clone();
                    tokio::spawn(async move {
                        while stream.recv().await.is_some() {
                            // First catchable signal: request the clean in-loop
                            // shutdown. A repeat after the grace window means
                            // that path is wedged (or a supervisor is
                            // escalating) — registering the handler replaced the
                            // default kill disposition, so swallowing repeats
                            // forever would leave an unkillable process. Exit
                            // hard with the conventional 128+signum code.
                            flag.store(true, Ordering::Relaxed);
                            let mut first = first_signal_at
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            match *first {
                                None => *first = Some(Instant::now()),
                                Some(at) if at.elapsed() > SIGNAL_ESCALATION_GRACE => {
                                    std::process::exit(128 + kind.as_raw_value());
                                }
                                Some(_) => {}
                            }
                        }
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to install termination signal handler");
                }
            }
        }
    }
    flag
}

/// Seed the current session's title from the user's first prompt when no title
/// exists yet, so the `/sessions` list shows more than the project name. The
/// agent's `set_session_title` tool can still refine it later; this only fills
/// the gap when the agent never sets one (common for non-Claude models).
async fn maybe_seed_session_title(app: &mut AppState, storage: &Storage, input: &str) {
    if !app.current_session_summary.trim().is_empty() {
        return;
    }
    let Some(session_id) = app.current_session_id else {
        return;
    };
    let Some(title) = derive_session_title(input) else {
        return;
    };
    if let Err(err) = storage.set_session_summary(session_id, &title).await {
        tracing::warn!(session_id = %session_id, error = %err, "failed to seed session title");
    }
    app.current_session_summary = title;
}

fn apply_ui_events_for_frame(
    app: &mut AppState,
    ui_receiver: &mut mpsc::UnboundedReceiver<UiEvent>,
    deferred_ui_event: &mut Option<UiEvent>,
    changed_files: &mut Vec<String>,
) -> bool {
    // A tool-start is either the per-call `ToolStarted` (headless/recording sinks)
    // or the batched `ToolCallsStarted` the TUI sink emits; match both so the
    // one-frame deferral below fires for the batched path too, not just the
    // (now production-dead) single-call one.
    let is_tool_start = |event: &UiEvent| {
        matches!(
            event,
            UiEvent::ToolStarted { .. } | UiEvent::ToolCallsStarted { .. }
        )
    };
    // Successful edit/write diffs feed the cross-session changed-files ledger
    // so live peers can see what this session touched.
    let collect_changed_file = |event: &UiEvent, changed_files: &mut Vec<String>| match event {
        UiEvent::ToolFinishedWithDiff {
            success: true,
            diff,
            ..
        } => changed_files.extend(diff.files().map(|file| file.path.clone())),
        UiEvent::WorkspaceChanged { paths } => {
            changed_files.extend(paths.iter().cloned());
        }
        _ => {}
    };
    let mut saw_tool_start = false;
    let mut changed = false;
    if let Some(event) = deferred_ui_event.take() {
        saw_tool_start = is_tool_start(&event);
        collect_changed_file(&event, changed_files);
        app.reduce(AppAction::Agent(event));
        changed = true;
    }

    while let Ok(event) = ui_receiver.try_recv() {
        let is_tool_start = is_tool_start(&event);
        if saw_tool_start && !is_tool_start {
            *deferred_ui_event = Some(event);
            break;
        }
        collect_changed_file(&event, changed_files);
        app.reduce(AppAction::Agent(event));
        changed = true;
        saw_tool_start |= is_tool_start;
    }
    changed
}

fn apply_interaction_request(
    app: &mut AppState,
    interaction_rx: &mut mpsc::UnboundedReceiver<InteractionRequest>,
) -> bool {
    if matches!(
        app.modal,
        Some(
            ModalKind::Onboarding { .. }
                | ModalKind::PermissionPrompt { .. }
                | ModalKind::SandboxEscalationPrompt { .. }
                | ModalKind::WebDomainPrompt { .. }
                | ModalKind::ExtensionToolPrompt { .. }
                | ModalKind::HookTrustPrompt { .. }
                | ModalKind::QuestionPrompt { .. }
        )
    ) {
        return false;
    }

    let Ok(request) = interaction_rx.try_recv() else {
        return false;
    };

    match request {
        InteractionRequest::Permission {
            request_id,
            command,
            origin,
        } => {
            app.pending_question_visibility = false;
            app.modal_scroll = 0;
            app.modal = Some(ModalKind::PermissionPrompt {
                request_id,
                command: command.clone(),
                origin,
            });
            app.focus = Focus::Modal;
            app.reduce(AppAction::ResetPermissionToolTimer {
                command,
                at: std::time::Instant::now(),
            });
        }
        InteractionRequest::SandboxEscalation {
            request_id,
            command,
            origin,
        } => {
            app.pending_question_visibility = false;
            app.modal_scroll = 0;
            app.modal = Some(ModalKind::SandboxEscalationPrompt {
                request_id,
                command,
                origin,
            });
            app.focus = Focus::Modal;
        }
        InteractionRequest::WebDomain {
            request_id,
            url,
            host,
            redirected_from,
            origin,
        } => {
            app.pending_question_visibility = false;
            app.modal_scroll = 0;
            app.modal = Some(ModalKind::WebDomainPrompt {
                request_id,
                url: url.clone(),
                host,
                redirected_from,
                origin,
            });
            app.focus = Focus::Modal;
            app.reduce(AppAction::ResetPermissionToolTimer {
                command: url,
                at: std::time::Instant::now(),
            });
        }
        InteractionRequest::ExtensionTool {
            request_id,
            id,
            server,
            capabilities,
            args_preview,
        } => {
            app.pending_question_visibility = false;
            app.modal_scroll = 0;
            app.modal = Some(ModalKind::ExtensionToolPrompt {
                request_id,
                id: id.clone(),
                server,
                capabilities,
                args_preview,
            });
            app.focus = Focus::Modal;
            app.reduce(AppAction::ResetPermissionToolTimer {
                command: id,
                at: std::time::Instant::now(),
            });
        }
        InteractionRequest::HookTrust {
            request_id,
            name,
            event,
            action_kind,
            action_preview,
        } => {
            app.pending_question_visibility = false;
            app.modal_scroll = 0;
            app.modal = Some(ModalKind::HookTrustPrompt {
                request_id,
                name: name.clone(),
                event,
                action_kind,
                action_preview,
            });
            app.focus = Focus::Modal;
            app.reduce(AppAction::ResetPermissionToolTimer {
                command: name,
                at: std::time::Instant::now(),
            });
        }
        InteractionRequest::Question {
            request_id,
            prompt,
            header,
            options,
            multiple,
            origin,
        } => {
            // Multi-select checkboxes open in each option's declared default
            // state, so a review-style question (/bug's section picker) can
            // start with its recommended set checked.
            let selected = options.iter().map(|option| option.preselected).collect();
            app.pending_question_visibility = false;
            app.modal_scroll = 0;
            app.modal = Some(ModalKind::QuestionPrompt {
                request_id,
                prompt,
                header,
                options,
                multiple,
                origin,
                cursor: 0,
                selected,
            });
            app.focus = Focus::Modal;
        }
    }
    true
}

fn apply_action_with_task_side_effects(
    action: AppAction,
    app: &mut AppState,
    tasks: &TaskController,
) {
    match action {
        AppAction::CancelQueuedInput { id } => {
            if let Err(err) = tasks.cancel_agent_message(id) {
                app.reduce(AppAction::Agent(UiEvent::Error(err)));
            }
            app.reduce(AppAction::CancelQueuedInput { id });
        }
        AppAction::CancelFocusedQueuedInput => {
            if let Some(id) = app.focused_queued_input_id() {
                if let Err(err) = tasks.cancel_agent_message(id) {
                    app.reduce(AppAction::Agent(UiEvent::Error(err)));
                }
                app.reduce(AppAction::CancelQueuedInput { id });
            }
        }
        AppAction::WithdrawQueuedInput => {
            if let Some(id) = app.last_queued_input().map(|queued| queued.id) {
                if let Err(err) = tasks.cancel_agent_message(id) {
                    app.reduce(AppAction::Agent(UiEvent::Error(err)));
                }
                app.reduce(AppAction::WithdrawQueuedInput);
            }
        }
        other => app.reduce(other),
    }
}

#[derive(Debug)]
struct DeferredCommandDeps<'a> {
    registry: Arc<ProviderRegistry>,
    model_catalog: Arc<ModelCatalog>,
    storage: Option<&'a crate::storage::Storage>,
}

async fn start_next_deferred_command_if_idle(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    session_store: Arc<Mutex<SessionStore>>,
    project_root: &Path,
    deps: DeferredCommandDeps<'_>,
) -> bool {
    if !matches!(app.task_state, TaskState::Idle) || tasks.is_busy() {
        return false;
    }
    let Some(command) = app.deferred_commands.first().cloned() else {
        return false;
    };

    match command.payload.clone() {
        crate::tui::app::DeferredCommandPayload::SlashCommand => {
            let input = command.input.clone();
            app.reduce(AppAction::SetTaskState(TaskState::Command));
            let command_deps = CommandTaskDeps::new(
                agent,
                session_store,
                project_root.to_path_buf(),
                deps.registry,
                deps.model_catalog,
                deps.storage.cloned(),
                app.credential_persistence,
            );
            if let Err(err) = tasks.start_command(
                input,
                app.transcript.to_vec(),
                app.command_generation(),
                command_deps,
            ) {
                app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
                return false;
            }
            app.reduce(AppAction::RemoveDeferredCommand { id: command.id });
        }
        crate::tui::app::DeferredCommandPayload::ModelSelection(selection) => {
            switch_model_selection(
                selection,
                app,
                agent,
                session_store,
                deps.registry,
                deps.model_catalog,
                tasks.runtime_sender(),
            );
            app.reduce(AppAction::RemoveDeferredCommand { id: command.id });
        }
    }
    true
}

/// Fold the live todo list's completed items into the plan canvas so completed
/// work survives a halt: `/continue` reseeds todos from the plan's task `done`
/// flags, and without this sync every task of the halted phase — including ones
/// the agent finished — came back pending. Returns the updated plan for the
/// caller's `app.plan` mirror, or `None` when nothing was completed.
async fn fold_completed_todos_into_plan(
    todo_store: &SharedTodoStore,
    plan_store: &SharedPlanStore,
) -> Option<crate::plan::PlanDoc> {
    let completed: Vec<String> = {
        let store = todo_store.lock().await;
        store
            .todos()
            .iter()
            .filter(|item| item.status == crate::todo::TodoStatus::Completed)
            .map(|item| item.content.clone())
            .collect()
    };
    if completed.is_empty() {
        return None;
    }
    let mut plan = plan_store.lock().await;
    let mut editor = plan.edit();
    for content in &completed {
        editor.check_task(content);
    }
    Some(plan.clone())
}

/// After a phased plan's run finishes, advance to the next pending phase or
/// halt. Mirrors `start_pending_queued_run_if_idle`: only acts when truly idle,
/// and is driven from the `poll_finished` site. Returns `true` when it started
/// the next phase's run so the caller skips the queued-run start.
async fn maybe_advance_plan_phase(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    todo_store: SharedTodoStore,
    plan_store: SharedPlanStore,
    repo_map: &mut RepoMapInjector,
) -> bool {
    if !matches!(app.task_state, TaskState::Idle) || tasks.is_busy() {
        return false;
    }
    let Some(advance) = app.phase_advance.take() else {
        return false;
    };
    let Some(execution) = app.plan_execution else {
        return false;
    };
    let current = execution.phase_index;

    if matches!(advance, PhaseAdvance::Halt) {
        // Preserve the position: fold the live todo list's completed items back
        // into the plan canvas before halting, so `/continue`'s
        // first-pending-phase scan resumes exactly where work stopped instead
        // of re-running tasks that already finished. Without this a halt at a
        // phase boundary (or mid-phase stop) left every task of the current
        // phase pending on the canvas — observed live as a run that completed
        // and verified its whole final phase, then was told to redo it.
        if let Some(plan) = fold_completed_todos_into_plan(&todo_store, &plan_store).await {
            app.plan = plan;
        }
        app.clear_plan_execution();
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Status,
            text: "Phase execution halted — type /continue to resume from the first pending phase."
                .to_string(),
        });
        return false;
    }

    // Mark the finished phase complete on the shared canvas (and its mirror) so
    // progress persists, then find the next phase that still has pending work.
    let plan = {
        let mut plan = plan_store.lock().await;
        plan.edit().mark_phase_done(current);
        plan.clone()
    };
    app.plan = plan.clone();

    let Some(next) = plan.next_phase_with_pending(Some(current)) else {
        app.clear_plan_execution();
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Status,
            text: "All phases complete.".to_string(),
        });
        return false;
    };

    // Seed only the next phase's todos and kick off its run — the phased
    // equivalent of implement_plan's flat handoff.
    let todos = plan.phase_todo_items(next);
    {
        let mut store = todo_store.lock().await;
        if todos.is_empty() {
            store.clear();
        } else {
            store.set_todos(todos.clone());
        }
    }
    app.todo = todos;
    app.plan_execution = Some(PlanExecution { phase_index: next });
    // Auto-advance follows the new active phase and scrolls its in-progress
    // item into view, even if the user had been browsing another phase.
    app.todo_phase_view = None;
    app.resting_todo_phase = None;
    app.scroll_todo_in_progress_into_view = true;

    let next_name = plan
        .phases
        .get(next)
        .map(|phase| phase.name.clone())
        .unwrap_or_default();
    app.reduce(AppAction::CommandOutput {
        kind: CommandOutputKind::Status,
        text: format!("Phase {} complete — starting {next_name}", current + 1),
    });
    app.reduce(AppAction::ScrollBottom);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    app.reduce(AppAction::Agent(UiEvent::Thinking(format!(
        "Implementing {next_name}"
    ))));
    sync_agent_self_review_mode(app.self_review_mode, &agent).await;
    if let Some(report) = repo_map.apply_before_turn(&agent).await {
        app.latest_context_report = Some(report);
    }
    if let Err(err) = tasks.start_implement_plan_with_context(
        agent,
        plan,
        sink,
        Some(next),
        crate::agent::PlanContextMode::Keep,
    ) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
        app.plan_execution = None;
        return false;
    }
    true
}

/// After the coding agent confirmed an `enter_plan_mode` switch, swap the
/// active persona to the requested mode and continue the current conversation
/// under it, so the model builds the plan on the canvas from the context it
/// already gathered rather than re-grounding. Mirrors `maybe_advance_plan_phase`:
/// acts only when idle and self-gates on `pending_persona_switch.take()`, so it
/// is a cheap no-op on every other frame. Returns `true` when it dispatched the
/// continuation run.
async fn maybe_apply_persona_switch(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    repo_map: &mut RepoMapInjector,
) -> bool {
    if !matches!(app.task_state, TaskState::Idle) || tasks.is_busy() {
        return false;
    }
    let Some(mode) = app.pending_persona_switch.take() else {
        return false;
    };
    let view = match mode {
        crate::agent::AgentMode::Planning => crate::tui::event::View::Plan,
        _ => crate::tui::event::View::Agent,
    };
    // SetView also sets `active_persona` to the view's default mode; the
    // continuation dispatch then applies that persona to the agent (swapping
    // the registry and injecting the transition system message).
    app.reduce(AppAction::SetView(view));
    let persona = app.active_persona.clone();
    app.reduce(AppAction::ScrollBottom);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    app.reduce(AppAction::Agent(UiEvent::Thinking(
        "Switched to plan mode — building the plan on the canvas".to_string(),
    )));
    sync_agent_self_review_mode(app.self_review_mode, &agent).await;
    if let Some(report) = repo_map.apply_before_turn(&agent).await {
        app.latest_context_report = Some(report);
    }
    if let Err(err) = tasks.start_persona_switch_continuation(agent, sink, persona) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
        return false;
    }
    true
}

async fn start_pending_queued_run_if_idle(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    repo_map: &mut RepoMapInjector,
    registry: &ProviderRegistry,
    model_catalog: &crate::model_catalog::ModelCatalog,
) -> bool {
    if !matches!(app.task_state, TaskState::Idle) || tasks.is_busy() {
        return false;
    }
    if app.first_queued_input().is_none() {
        return false;
    }

    let mut queued_messages = app
        .queued_inputs
        .iter()
        .map(|queued| QueuedUserMessage {
            id: queued.id,
            display_text: queued.text.clone(),
            input: queued.content.submission().input,
        })
        .collect::<Vec<_>>();
    if !crate::model_resolution::model_supports_vision(
        registry,
        Some(model_catalog),
        &app.provider,
        &app.model,
    ) {
        let blocked_ids = queued_messages
            .iter()
            .filter(|message| message.input.has_images())
            .map(|message| message.id)
            .collect::<Vec<_>>();
        if !blocked_ids.is_empty() {
            for id in &blocked_ids {
                app.reduce(AppAction::CancelQueuedInput { id: *id });
            }
            push_command_message(
                app,
                CommandOutputKind::Error,
                "This model can't see images — queued image message not sent.",
            );
            queued_messages.retain(|message| !blocked_ids.contains(&message.id));
        }
    }
    if queued_messages.is_empty() {
        return false;
    }
    // Pop the primary message out of the pre-seed list: it opens the run via
    // `begin_run`, so leaving it in `queued_messages` would deliver it a
    // second time when the run loop drains the queue channel (duplicate
    // transcript row + duplicate model input).
    let queued = queued_messages.remove(0);

    app.reduce(AppAction::Agent(UiEvent::QueuedUserMessageSent {
        id: queued.id,
        text: queued.display_text.clone(),
    }));
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    let persona = app.active_persona.clone();
    let phase = app.active_mode().run_status_label();
    app.reduce(AppAction::Agent(UiEvent::Thinking(format!(
        "{phase}: {}",
        summarize_input(&queued.display_text)
    ))));
    // The first queued message is the run's opening turn; expand its snapshot to
    // the model payload (chips spliced in, images attached).
    let primary_input = queued.input;
    sync_agent_self_review_mode(app.self_review_mode, &agent).await;
    if let Some(report) = repo_map.apply_before_turn(&agent).await {
        app.latest_context_report = Some(report);
    }
    if let Err(err) =
        tasks.start_agent_run_with_queued(agent, primary_input, sink, persona, queued_messages)
    {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
    }
    true
}

/// Build the wizard state for `/wizard [provider-id]`: a blank create flow
/// without an argument, or a prefilled edit flow when the id names an
/// existing wizard-managed provider.
fn local_model_wizard_state_for_input(
    input: &str,
    model_catalog: &crate::model_catalog::ModelCatalog,
    home_dir: &std::path::Path,
    credential_persistence: crate::session::CredentialPersistence,
) -> Result<crate::tui::local_model_wizard::LocalModelWizardState, String> {
    let argument = input.strip_prefix("/wizard").unwrap_or_default().trim();
    if argument.is_empty() {
        return Ok(
            crate::tui::local_model_wizard::LocalModelWizardState::with_persistence(
                credential_persistence,
            ),
        );
    }
    crate::tui::local_model_wizard::wizard_state_for_provider_id(
        argument,
        model_catalog,
        home_dir,
        credential_persistence,
    )
}

mod background;
mod scroll;

#[cfg(test)]
mod tests;
