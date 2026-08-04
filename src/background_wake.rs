//! Runtime-owned waits for process-local background tasks and PTYs.
//!
//! The coordinator persists one-shot subscriptions, but intentionally keeps OS
//! handles in their owning registries. A restarted runtime can therefore never
//! attach an old wait to a reused `bg-N` or `pty-N` identifier.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::broadcast;

use crate::background::{BackgroundTaskRegistry, BackgroundTaskSnapshot, BackgroundTaskStatus};
use crate::storage::{
    BackgroundWakeRegistration, BackgroundWakeTargetKind, BackgroundWakeTrigger, Storage,
};
use crate::terminal::{TerminalPromptState, TerminalRegistry, TerminalSnapshot, TerminalStatus};
use crate::tool::SharedActiveSessionId;
use crate::util::time::now_ms;

/// Exact identity returned to the agent when it parks on local work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundWorkWait {
    pub(crate) subscription_id: i64,
    pub(crate) requester_generation: i64,
    pub(crate) target_kind: BackgroundWakeTargetKind,
    pub(crate) target_id: String,
    pub(crate) target_incarnation: String,
    pub(crate) observed_version: u64,
}

/// Result of atomically establishing a wait from the terminal's current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalWaitRegistration {
    /// The requested observation is already stale or the terminal is ready.
    Ready(TerminalSnapshot),
    /// A durable one-shot wait was established from the current semantic version.
    Parked(BackgroundWorkWait),
}

/// The one-shot wake delivered once a qualifying registry observation changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundWorkWake {
    pub(crate) wait: BackgroundWorkWait,
    pub(crate) reason: String,
    pub(crate) wake_version: u64,
    pub(crate) output: String,
    pub(crate) output_truncated: bool,
}

#[derive(Debug, Clone, Copy)]
struct WorkWaitRegistration<'a> {
    target_kind: BackgroundWakeTargetKind,
    target_id: &'a str,
    target_incarnation: &'a str,
    observed_version: u64,
    output_threshold: Option<usize>,
    wait_seconds: Option<u64>,
    operation_key: &'a str,
}

/// Shared runtime service that turns registry events into durable, exact wakes.
#[derive(Debug)]
pub(crate) struct BackgroundWakeCoordinator {
    storage: Storage,
    runtime_owner_id: String,
    active_session_id: SharedActiveSessionId,
    background_tasks: Arc<BackgroundTaskRegistry>,
    terminals: Arc<TerminalRegistry>,
    events: broadcast::Sender<BackgroundWorkWake>,
    next_generation: AtomicI64,
}

impl BackgroundWakeCoordinator {
    pub(crate) fn new(
        storage: Storage,
        active_session_id: SharedActiveSessionId,
        background_tasks: Arc<BackgroundTaskRegistry>,
        terminals: Arc<TerminalRegistry>,
    ) -> Self {
        let (events, _) = broadcast::channel(128);
        let coordinator = Self {
            storage,
            runtime_owner_id: uuid::Uuid::now_v7().to_string(),
            active_session_id,
            background_tasks,
            terminals,
            events,
            next_generation: AtomicI64::new(1),
        };
        coordinator.spawn_stale_wait_reaper();
        coordinator
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<BackgroundWorkWake> {
        self.events.subscribe()
    }

    fn spawn_stale_wait_reaper(&self) {
        let storage = self.storage.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                if let Err(error) = storage.cancel_stale_background_wakes().await {
                    tracing::warn!(%error, "failed to invalidate stale process-local background waits");
                }
            }
        });
    }

    pub(crate) async fn register_background_task(
        self: &Arc<Self>,
        snapshot: BackgroundTaskSnapshot,
        operation_key: &str,
        observed_version: u64,
        output_threshold: Option<usize>,
        wait_seconds: Option<u64>,
    ) -> Result<BackgroundWorkWait> {
        if snapshot.version != observed_version {
            bail!(
                "Background task {} advanced from observed version {} to {}; tail it and choose a new wait",
                snapshot.id,
                observed_version,
                snapshot.version
            );
        }
        if snapshot.status.is_finished() {
            bail!(
                "Background task {} is already {}",
                snapshot.id,
                snapshot.status.label()
            );
        }
        validate_output_threshold(
            "Background task",
            &snapshot.id,
            output_threshold,
            snapshot.total_output_chars,
        )?;
        self.register(WorkWaitRegistration {
            target_kind: BackgroundWakeTargetKind::BackgroundTask,
            target_id: &snapshot.id,
            target_incarnation: &snapshot.incarnation,
            observed_version,
            output_threshold,
            wait_seconds,
            operation_key,
        })
        .await
    }

    pub(crate) async fn register_terminal_current(
        self: &Arc<Self>,
        terminal_id: &str,
        operation_key: &str,
        observed_version: Option<u64>,
        output_threshold: Option<usize>,
        wait_seconds: Option<u64>,
    ) -> Result<TerminalWaitRegistration> {
        let snapshot = self
            .terminals
            .snapshot(terminal_id)
            .await
            .with_context(|| format!("Unknown interactive terminal: {terminal_id}"))?;
        let observation_advanced =
            observed_version.is_some_and(|version| version != snapshot.version);
        let threshold_reached =
            output_threshold.is_some_and(|threshold| snapshot.total_output_chars >= threshold);
        if snapshot.status.is_finished()
            || snapshot.prompt_state == TerminalPromptState::WaitingForInput
            || observation_advanced
            || threshold_reached
        {
            return Ok(TerminalWaitRegistration::Ready(snapshot));
        }
        let wait = self
            .register(WorkWaitRegistration {
                target_kind: BackgroundWakeTargetKind::Terminal,
                target_id: &snapshot.id,
                target_incarnation: &snapshot.incarnation,
                observed_version: snapshot.version,
                output_threshold,
                wait_seconds,
                operation_key,
            })
            .await?;
        Ok(TerminalWaitRegistration::Parked(wait))
    }

    async fn register(
        self: &Arc<Self>,
        registration: WorkWaitRegistration<'_>,
    ) -> Result<BackgroundWorkWait> {
        let requester = (*self.active_session_id.lock().await)
            .context("Background waits require an active session")?;
        let requester_generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let deadline_at_ms = registration
            .wait_seconds
            .map(|seconds| i64::try_from(seconds).context("Background wait duration overflow"))
            .transpose()?
            .and_then(|seconds| seconds.checked_mul(1_000))
            .and_then(|milliseconds| now_ms().checked_add(milliseconds));
        let persisted = self
            .storage
            .register_background_wake(BackgroundWakeRegistration {
                requester,
                requester_generation,
                owner_runtime_id: &self.runtime_owner_id,
                operation_key: registration.operation_key,
                target_kind: registration.target_kind,
                target_id: registration.target_id,
                target_incarnation: registration.target_incarnation,
                observed_version: registration.observed_version,
                output_threshold: registration.output_threshold,
                deadline_at_ms,
            })
            .await?;
        let wait = BackgroundWorkWait {
            subscription_id: persisted.wait.subscription_id,
            requester_generation: persisted.wait.requester_generation,
            target_kind: persisted.target_kind,
            target_id: persisted.target_id,
            target_incarnation: persisted.target_incarnation,
            observed_version: persisted.observed_version,
        };
        if persisted.newly_created {
            self.clone().spawn_watch(
                wait.clone(),
                persisted.output_threshold,
                persisted.deadline_at_ms,
            );
        }
        Ok(wait)
    }

    fn spawn_watch(
        self: Arc<Self>,
        wait: BackgroundWorkWait,
        output_threshold: Option<usize>,
        deadline_at_ms: Option<i64>,
    ) {
        tokio::spawn(async move {
            if let Err(error) = self.watch(wait, output_threshold, deadline_at_ms).await {
                tracing::warn!(%error, "background wake watcher stopped");
            }
        });
    }

    async fn watch(
        &self,
        wait: BackgroundWorkWait,
        output_threshold: Option<usize>,
        deadline_at_ms: Option<i64>,
    ) -> Result<()> {
        match wait.target_kind {
            BackgroundWakeTargetKind::BackgroundTask => {
                self.watch_background_task(wait, output_threshold, deadline_at_ms)
                    .await
            }
            BackgroundWakeTargetKind::Terminal => {
                self.watch_terminal(wait, output_threshold, deadline_at_ms)
                    .await
            }
        }
    }

    async fn watch_background_task(
        &self,
        wait: BackgroundWorkWait,
        output_threshold: Option<usize>,
        deadline_at_ms: Option<i64>,
    ) -> Result<()> {
        let mut events = self.background_tasks.subscribe();
        if self.try_fire(&wait, output_threshold).await? {
            return Ok(());
        }
        loop {
            let deadline = remaining_deadline(deadline_at_ms);
            tokio::select! {
                result = events.recv() => match result {
                    Ok(event) if event.task_id() == wait.target_id => {
                        let _ = event.version();
                        if self.try_fire(&wait, output_threshold).await? {
                            return Ok(());
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if self.try_fire(&wait, output_threshold).await? {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                () = sleep_until_deadline(deadline), if deadline.is_some() => {
                    self.fire_due().await?;
                    return Ok(());
                }
            }
        }
    }

    async fn watch_terminal(
        &self,
        wait: BackgroundWorkWait,
        output_threshold: Option<usize>,
        deadline_at_ms: Option<i64>,
    ) -> Result<()> {
        let mut events = self.terminals.subscribe();
        if self.try_fire(&wait, output_threshold).await? {
            return Ok(());
        }
        loop {
            let deadline = remaining_deadline(deadline_at_ms);
            tokio::select! {
                result = events.recv() => match result {
                    Ok(event)
                        if event.terminal_id() == wait.target_id
                            && terminal_event_requires_check(&event, output_threshold) =>
                    {
                        let _ = event.version();
                        if self.try_fire(&wait, output_threshold).await? {
                            return Ok(());
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if self.try_fire(&wait, output_threshold).await? {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                () = sleep_until_deadline(deadline), if deadline.is_some() => {
                    self.fire_due().await?;
                    return Ok(());
                }
            }
        }
    }

    async fn try_fire(
        &self,
        wait: &BackgroundWorkWait,
        output_threshold: Option<usize>,
    ) -> Result<bool> {
        let observation = match wait.target_kind {
            BackgroundWakeTargetKind::BackgroundTask => self
                .background_tasks
                .snapshot(&wait.target_id)
                .await
                .map(|snapshot| {
                    (
                        snapshot.incarnation.clone(),
                        background_observation(snapshot),
                    )
                }),
            BackgroundWakeTargetKind::Terminal => self
                .terminals
                .snapshot(&wait.target_id)
                .await
                .map(|snapshot| (snapshot.incarnation.clone(), terminal_observation(snapshot))),
        };
        let Some((incarnation, observation)) = observation else {
            self.fire(BackgroundWakeTrigger {
                target_kind: wait.target_kind,
                target_id: &wait.target_id,
                target_incarnation: &wait.target_incarnation,
                wake_reason: "removed",
                wake_version: wait.observed_version.saturating_add(1),
                total_output_chars: 0,
                output: "",
                output_truncated: false,
            })
            .await?;
            return Ok(true);
        };
        if incarnation != wait.target_incarnation {
            self.fire(BackgroundWakeTrigger {
                target_kind: wait.target_kind,
                target_id: &wait.target_id,
                target_incarnation: &wait.target_incarnation,
                wake_reason: "owner_lost",
                wake_version: wait.observed_version.saturating_add(1),
                total_output_chars: 0,
                output: "",
                output_truncated: false,
            })
            .await?;
            return Ok(true);
        }
        let threshold_reached =
            output_threshold.is_some_and(|threshold| observation.total_output_chars >= threshold);
        if observation.version <= wait.observed_version && !threshold_reached {
            return Ok(false);
        }
        let is_relevant = observation.finished
            || observation.input_required
            || output_threshold.is_none()
            || threshold_reached;
        if !is_relevant {
            return Ok(false);
        }
        self.fire(BackgroundWakeTrigger {
            target_kind: wait.target_kind,
            target_id: &wait.target_id,
            target_incarnation: &wait.target_incarnation,
            wake_reason: if threshold_reached
                && !observation.finished
                && !observation.input_required
            {
                "output_threshold"
            } else {
                observation.reason
            },
            wake_version: observation.version,
            total_output_chars: observation.total_output_chars,
            output: &observation.output,
            output_truncated: observation.output_truncated,
        })
        .await?;
        Ok(true)
    }

    async fn fire_due(&self) -> Result<()> {
        let deliveries = self
            .storage
            .fire_due_background_wakes(&self.runtime_owner_id)
            .await?;
        for delivery in deliveries {
            let _ = self.events.send(BackgroundWorkWake {
                wait: BackgroundWorkWait {
                    subscription_id: delivery.wait.subscription_id,
                    requester_generation: delivery.wait.requester_generation,
                    target_kind: delivery.target_kind,
                    target_id: delivery.target_id,
                    target_incarnation: delivery.target_incarnation,
                    observed_version: delivery.observed_version,
                },
                reason: delivery.wake_reason,
                wake_version: delivery.wake_version,
                output: delivery.output,
                output_truncated: delivery.output_truncated,
            });
        }
        Ok(())
    }

    async fn fire(&self, trigger: BackgroundWakeTrigger<'_>) -> Result<()> {
        let deliveries = self
            .storage
            .fire_background_wakes(&self.runtime_owner_id, trigger)
            .await?;
        for delivery in deliveries {
            let _ = self.events.send(BackgroundWorkWake {
                wait: BackgroundWorkWait {
                    subscription_id: delivery.wait.subscription_id,
                    requester_generation: delivery.wait.requester_generation,
                    target_kind: delivery.target_kind,
                    target_id: delivery.target_id,
                    target_incarnation: delivery.target_incarnation,
                    observed_version: delivery.observed_version,
                },
                reason: delivery.wake_reason,
                wake_version: delivery.wake_version,
                output: delivery.output,
                output_truncated: delivery.output_truncated,
            });
        }
        Ok(())
    }
}

fn remaining_deadline(deadline_at_ms: Option<i64>) -> Option<Duration> {
    deadline_at_ms.map(|timestamp| {
        Duration::from_millis(u64::try_from(timestamp.saturating_sub(now_ms())).unwrap_or_default())
    })
}

async fn sleep_until_deadline(deadline: Option<Duration>) {
    if let Some(duration) = deadline {
        tokio::time::sleep(duration).await;
    }
}

fn terminal_event_requires_check(
    event: &crate::terminal::TerminalEvent,
    output_threshold: Option<usize>,
) -> bool {
    match event {
        crate::terminal::TerminalEvent::Output {
            semantic_changed, ..
        } => *semantic_changed || output_threshold.is_some(),
        crate::terminal::TerminalEvent::Started { .. } => false,
        crate::terminal::TerminalEvent::WaitingForInput { .. }
        | crate::terminal::TerminalEvent::Resized { .. }
        | crate::terminal::TerminalEvent::Finished { .. }
        | crate::terminal::TerminalEvent::Removed { .. } => true,
    }
}

fn validate_output_threshold(
    target_kind: &str,
    target_id: &str,
    output_threshold: Option<usize>,
    observed_output_chars: usize,
) -> Result<()> {
    if output_threshold.is_some_and(|threshold| threshold <= observed_output_chars) {
        bail!(
            "{target_kind} {target_id} already has {observed_output_chars} output characters; output_threshold must be greater than the observed total"
        );
    }
    Ok(())
}

struct WorkObservation {
    version: u64,
    total_output_chars: usize,
    finished: bool,
    input_required: bool,
    reason: &'static str,
    output: String,
    output_truncated: bool,
}

fn background_observation(snapshot: BackgroundTaskSnapshot) -> WorkObservation {
    let reason = match snapshot.status {
        BackgroundTaskStatus::Running => "output_threshold",
        BackgroundTaskStatus::Succeeded => "succeeded",
        BackgroundTaskStatus::Failed => "failed",
        BackgroundTaskStatus::TimedOut => "task_timeout",
        BackgroundTaskStatus::Stopped => "cancelled",
    };
    WorkObservation {
        version: snapshot.version,
        total_output_chars: snapshot.total_output_chars,
        finished: snapshot.status.is_finished(),
        input_required: false,
        reason,
        output: snapshot.tail,
        output_truncated: snapshot.tail_truncated,
    }
}

fn terminal_observation(snapshot: TerminalSnapshot) -> WorkObservation {
    let reason = match snapshot.status {
        TerminalStatus::Running
            if snapshot.prompt_state == TerminalPromptState::WaitingForInput =>
        {
            "input_required"
        }
        TerminalStatus::Running => "screen_changed",
        TerminalStatus::Succeeded => "succeeded",
        TerminalStatus::Failed => "failed",
        TerminalStatus::TimedOut => "task_timeout",
        TerminalStatus::Stopped => "cancelled",
    };
    let (output, output_truncated) = snapshot.wake_output();
    WorkObservation {
        version: snapshot.version,
        total_output_chars: snapshot.total_output_chars,
        finished: snapshot.status.is_finished(),
        input_required: snapshot.prompt_state == TerminalPromptState::WaitingForInput,
        reason,
        output,
        output_truncated,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::Mutex;

    use super::{BackgroundWakeCoordinator, TerminalWaitRegistration};
    use crate::background::BackgroundTaskRegistry;
    use crate::storage::test_utils::TestStorage;
    use crate::terminal::{TerminalRegistry, TerminalSnapshot};

    const INITIAL_FRAME: &str = "\u{1b}[2J\u{1b}[H⠋ bonsai\r\n⣾ read · 100ms\r\n⌂ bonsai   ⏱ 9s";
    const COSMETIC_FRAME: &str = "\u{1b}[2J\u{1b}[H⠙ bonsai\r\n⣽ read · 8.4s\r\n⌂ bonsai   ⏱ 1m 5s";

    struct WakeFixture {
        storage: TestStorage,
        terminals: Arc<TerminalRegistry>,
        coordinator: Arc<BackgroundWakeCoordinator>,
    }

    impl WakeFixture {
        async fn new() -> Self {
            let storage = TestStorage::new().await;
            let session_id = storage.start_session().await;
            let active_session_id = Arc::new(Mutex::new(None));
            crate::storage::activate_session_heartbeat(
                &storage.storage,
                &active_session_id,
                session_id,
            )
            .await
            .expect("test session should become live");
            let terminals = Arc::new(TerminalRegistry::new());
            let coordinator = Arc::new(BackgroundWakeCoordinator::new(
                storage.storage.clone(),
                active_session_id,
                Arc::new(BackgroundTaskRegistry::new()),
                terminals.clone(),
            ));
            Self {
                storage,
                terminals,
                coordinator,
            }
        }

        async fn start_terminal(&self, command: &str) -> TerminalSnapshot {
            self.terminals
                .start("/bin/sh", command, self.storage.project_path(), 60, None)
                .await
                .expect("PTY fixture should start")
        }

        async fn start_sleep(&self) -> TerminalSnapshot {
            self.start_terminal("sleep 30").await
        }
    }

    fn parked(registration: TerminalWaitRegistration) -> super::BackgroundWorkWait {
        match registration {
            TerminalWaitRegistration::Parked(wait) => wait,
            TerminalWaitRegistration::Ready(snapshot) => {
                panic!("terminal unexpectedly ready: {snapshot:?}")
            }
        }
    }

    #[tokio::test]
    async fn stale_observation_returns_current_screen_without_a_retry_error() {
        let fixture = WakeFixture::new().await;
        let started = fixture.start_sleep().await;
        fixture
            .terminals
            .append_test_output(&started.id, "real output")
            .await;

        let registration = fixture
            .coordinator
            .register_terminal_current(
                &started.id,
                "stale-observation",
                Some(started.version),
                None,
                None,
            )
            .await
            .expect("stale observation should return the current screen");
        let TerminalWaitRegistration::Ready(current) = registration else {
            panic!("stale observation must not park")
        };
        assert!(current.version > started.version);
        assert!(current.screen.contains("real output"), "{}", current.screen);

        fixture
            .terminals
            .stop(&started.id)
            .await
            .expect("PTY fixture should stop");
    }

    #[tokio::test]
    async fn current_input_prompt_returns_ready_instead_of_parking_forever() {
        let fixture = WakeFixture::new().await;
        let started = fixture.start_terminal("printf 'Name: '; read answer").await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let current = fixture
                .terminals
                .snapshot(&started.id)
                .await
                .expect("PTY fixture should remain registered");
            if current.prompt_state == crate::terminal::TerminalPromptState::WaitingForInput {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let registration = fixture
            .coordinator
            .register_terminal_current(&started.id, "current-prompt", None, None, None)
            .await
            .expect("current prompt should be returned");
        assert!(matches!(registration, TerminalWaitRegistration::Ready(_)));

        fixture
            .terminals
            .stop(&started.id)
            .await
            .expect("PTY fixture should stop");
    }

    #[tokio::test]
    async fn cosmetic_redraws_stay_parked_until_terminal_cancellation() {
        let fixture = WakeFixture::new().await;
        let started = fixture.start_sleep().await;
        fixture
            .terminals
            .append_test_output(&started.id, INITIAL_FRAME)
            .await;
        let baseline = fixture
            .terminals
            .snapshot(&started.id)
            .await
            .expect("PTY fixture should remain registered");
        let mut wakes = fixture.coordinator.subscribe();
        let wait = parked(
            fixture
                .coordinator
                .register_terminal_current(&started.id, "cosmetic-redraws", None, None, None)
                .await
                .expect("wait should register from the current screen"),
        );

        for _ in 0..20 {
            fixture
                .terminals
                .append_test_output(&started.id, COSMETIC_FRAME)
                .await;
        }
        let after_redraws = fixture
            .terminals
            .snapshot(&started.id)
            .await
            .expect("PTY fixture should remain registered");
        assert_eq!(after_redraws.version, wait.observed_version);
        assert_eq!(after_redraws.version, baseline.version);
        assert!(after_redraws.total_output_chars > baseline.total_output_chars);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), wakes.recv())
                .await
                .is_err(),
            "cosmetic redraws must not wake the agent"
        );

        fixture
            .terminals
            .stop(&started.id)
            .await
            .expect("PTY fixture should stop");
        let wake = tokio::time::timeout(Duration::from_secs(2), wakes.recv())
            .await
            .expect("cancellation should wake immediately")
            .expect("wake channel should remain open");
        assert_eq!(wake.reason, "cancelled");
        assert!(wake.wake_version > wake.wait.observed_version);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), wakes.recv())
                .await
                .is_err(),
            "a terminal wait must wake only once"
        );
    }

    #[tokio::test]
    async fn semantic_change_after_registration_is_not_lost() {
        let fixture = WakeFixture::new().await;
        let started = fixture.start_sleep().await;
        fixture
            .terminals
            .append_test_output(&started.id, "phase one")
            .await;
        let baseline = fixture
            .terminals
            .snapshot(&started.id)
            .await
            .expect("PTY fixture should remain registered");
        let mut wakes = fixture.coordinator.subscribe();
        let wait = parked(
            fixture
                .coordinator
                .register_terminal_current(
                    &started.id,
                    "semantic-change",
                    Some(baseline.version),
                    None,
                    None,
                )
                .await
                .expect("wait should register"),
        );

        // Deliberately emit before the spawned watcher is guaranteed to have
        // subscribed. Its initial snapshot recheck must close this race.
        fixture
            .terminals
            .append_test_output(&started.id, "\rphase two")
            .await;
        let wake = tokio::time::timeout(Duration::from_secs(2), wakes.recv())
            .await
            .expect("semantic output should wake")
            .expect("wake channel should remain open");
        assert_eq!(wake.reason, "screen_changed");
        assert_eq!(wake.wait.subscription_id, wait.subscription_id);
        assert!(wake.wake_version > baseline.version);
        assert!(wake.output.contains("phase two"), "{}", wake.output);

        fixture
            .terminals
            .stop(&started.id)
            .await
            .expect("PTY fixture should stop");
    }

    #[tokio::test]
    async fn raw_output_threshold_survives_cosmetic_coalescing() {
        let fixture = WakeFixture::new().await;
        let started = fixture.start_sleep().await;
        fixture
            .terminals
            .append_test_output(&started.id, INITIAL_FRAME)
            .await;
        let baseline = fixture
            .terminals
            .snapshot(&started.id)
            .await
            .expect("PTY fixture should remain registered");
        let mut wakes = fixture.coordinator.subscribe();
        parked(
            fixture
                .coordinator
                .register_terminal_current(
                    &started.id,
                    "raw-threshold",
                    Some(baseline.version),
                    Some(baseline.total_output_chars + 1),
                    None,
                )
                .await
                .expect("threshold wait should register"),
        );

        fixture
            .terminals
            .append_test_output(&started.id, COSMETIC_FRAME)
            .await;
        let wake = tokio::time::timeout(Duration::from_secs(2), wakes.recv())
            .await
            .expect("raw threshold should wake")
            .expect("wake channel should remain open");
        assert_eq!(wake.reason, "output_threshold");
        assert_eq!(wake.wake_version, baseline.version);

        fixture
            .terminals
            .stop(&started.id)
            .await
            .expect("PTY fixture should stop");
    }

    #[tokio::test]
    async fn cosmetic_activity_does_not_extend_the_wait_deadline() {
        let fixture = WakeFixture::new().await;
        let started = fixture.start_sleep().await;
        fixture
            .terminals
            .append_test_output(&started.id, INITIAL_FRAME)
            .await;
        let mut wakes = fixture.coordinator.subscribe();
        parked(
            fixture
                .coordinator
                .register_terminal_current(&started.id, "fixed-deadline", None, None, Some(1))
                .await
                .expect("deadline wait should register"),
        );
        let registered_at = Instant::now();

        for _ in 0..8 {
            fixture
                .terminals
                .append_test_output(&started.id, COSMETIC_FRAME)
                .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let wake = tokio::time::timeout(Duration::from_secs(1), wakes.recv())
            .await
            .expect("deadline should survive cosmetic activity")
            .expect("wake channel should remain open");
        assert_eq!(wake.reason, "deadline");
        assert!(registered_at.elapsed() < Duration::from_millis(1_500));

        fixture
            .terminals
            .stop(&started.id)
            .await
            .expect("PTY fixture should stop");
    }

    #[tokio::test]
    async fn process_exit_wakes_a_parked_terminal_immediately() {
        let fixture = WakeFixture::new().await;
        let started = fixture.start_terminal("sleep 0.2").await;
        let mut wakes = fixture.coordinator.subscribe();
        parked(
            fixture
                .coordinator
                .register_terminal_current(&started.id, "process-exit", None, None, None)
                .await
                .expect("wait should register"),
        );

        let wake = tokio::time::timeout(Duration::from_secs(2), wakes.recv())
            .await
            .expect("process exit should wake immediately")
            .expect("wake channel should remain open");
        assert_eq!(wake.reason, "succeeded");
        assert!(wake.wake_version > wake.wait.observed_version);
    }
}
