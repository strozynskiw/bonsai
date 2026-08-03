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

    pub(crate) async fn register_terminal(
        self: &Arc<Self>,
        snapshot: TerminalSnapshot,
        operation_key: &str,
        observed_version: u64,
        output_threshold: Option<usize>,
        wait_seconds: Option<u64>,
    ) -> Result<BackgroundWorkWait> {
        if snapshot.version != observed_version {
            bail!(
                "Interactive terminal {} advanced from observed version {} to {}; read it and choose a new wait",
                snapshot.id,
                observed_version,
                snapshot.version
            );
        }
        if snapshot.status.is_finished() {
            bail!(
                "Interactive terminal {} is already {}",
                snapshot.id,
                snapshot.status.label()
            );
        }
        validate_output_threshold(
            "Interactive terminal",
            &snapshot.id,
            output_threshold,
            snapshot.total_output_chars,
        )?;
        self.register(WorkWaitRegistration {
            target_kind: BackgroundWakeTargetKind::Terminal,
            target_id: &snapshot.id,
            target_incarnation: &snapshot.incarnation,
            observed_version,
            output_threshold,
            wait_seconds,
            operation_key,
        })
        .await
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
        loop {
            let deadline = deadline_at_ms.map(|timestamp| {
                Duration::from_millis(
                    u64::try_from(timestamp.saturating_sub(now_ms())).unwrap_or(0),
                )
            });
            match wait.target_kind {
                BackgroundWakeTargetKind::BackgroundTask => {
                    let mut events = self.background_tasks.subscribe();
                    if self.try_fire(&wait, output_threshold).await? {
                        return Ok(());
                    }
                    tokio::select! {
                        result = events.recv() => {
                            match result {
                                Ok(event) if event.task_id() == wait.target_id => {
                                    let _ = event.version();
                                }
                                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                            }
                        }
                        () = async { if let Some(duration) = deadline { tokio::time::sleep(duration).await; } }, if deadline.is_some() => {
                            self.fire_due().await?;
                            return Ok(());
                        }
                    }
                }
                BackgroundWakeTargetKind::Terminal => {
                    let mut events = self.terminals.subscribe();
                    if self.try_fire(&wait, output_threshold).await? {
                        return Ok(());
                    }
                    tokio::select! {
                        result = events.recv() => {
                            match result {
                                Ok(event) if event.terminal_id() == wait.target_id => {
                                    let _ = event.version();
                                }
                                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                            }
                        }
                        () = async { if let Some(duration) = deadline { tokio::time::sleep(duration).await; } }, if deadline.is_some() => {
                            self.fire_due().await?;
                            return Ok(());
                        }
                    }
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
        if observation.version <= wait.observed_version {
            return Ok(false);
        }
        let is_relevant = observation.finished
            || observation.input_required
            || output_threshold.is_none()
            || output_threshold
                .is_some_and(|threshold| observation.total_output_chars >= threshold);
        if !is_relevant {
            return Ok(false);
        }
        self.fire(BackgroundWakeTrigger {
            target_kind: wait.target_kind,
            target_id: &wait.target_id,
            target_incarnation: &wait.target_incarnation,
            wake_reason: observation.reason,
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
        TerminalStatus::Running => "output_threshold",
        TerminalStatus::Succeeded => "succeeded",
        TerminalStatus::Failed => "failed",
        TerminalStatus::TimedOut => "task_timeout",
        TerminalStatus::Stopped => "cancelled",
    };
    WorkObservation {
        version: snapshot.version,
        total_output_chars: snapshot.total_output_chars,
        finished: snapshot.status.is_finished(),
        input_required: snapshot.prompt_state == TerminalPromptState::WaitingForInput,
        reason,
        output: snapshot.tail,
        output_truncated: snapshot.tail_truncated,
    }
}
