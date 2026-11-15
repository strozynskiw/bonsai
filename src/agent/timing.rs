//! The agent's handle on the run timing ledger (issue #146).
//!
//! [`crate::timing`] owns the interval arithmetic and
//! [`crate::storage::timing`](crate::storage) owns its durability; this module is
//! the small agent-side surface between them: re-anchor the run clock at the
//! start of a task run, and hand the increments to storage when the run reaches
//! a checkpoint.

use super::Agent;
use crate::timing::{SharedTimingLedger, TimingCheckpoint, TimingPolicy};

/// Durable session time this process starts from, per clock.
///
/// The ledger only knows the active run, so a resumed session's already-spent
/// budget has to come from storage: `sessions.active_compute_ms` for the default
/// policy and `sessions.active_run_ms` for the `wall_clock` opt-in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RestoredSessionTime {
    /// Model, active tool, and child-active time recorded by earlier segments.
    pub(crate) active_compute_ms: u64,
    /// The heartbeat union earlier segments spent, waits included.
    pub(crate) active_run_ms: u64,
}

/// Cumulative session time, split by what it was spent on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionTimeReading {
    /// Which clock the session's active-time budget consumes.
    pub(crate) policy: TimingPolicy,
    /// Spent time on that clock: this is the reading a budget compares against.
    pub(crate) used_ms: u64,
    /// Model, active tool, and child-active time across the session.
    pub(crate) active_compute_ms: u64,
    /// Queue, permission, user, and child-wait time across the session.
    pub(crate) waiting_ms: u64,
}

impl SessionTimeReading {
    /// Whether this reading can exhaust `limit_seconds`, and the evidence why.
    ///
    /// Shared by enforcement and the alert surface so both answer "how much of
    /// the session budget is gone" from the same arithmetic.
    pub(crate) const fn exhausted_at(self, limit_seconds: u64) -> Option<(u64, u64)> {
        let limit_ms = limit_seconds.saturating_mul(1_000);
        if self.used_ms < limit_ms {
            return None;
        }
        Some((limit_seconds, self.used_ms.div_ceil(1_000)))
    }
}

impl Agent {
    /// Install the runtime's shared ledger.
    ///
    /// The runtime owns ledger creation because two producers need the same one:
    /// the agent records model, tool, and child intervals, while the interaction
    /// service records the permission and user waits that happen inside tool
    /// execution.
    ///
    /// An agent built outside the runtime (a test, an eval harness, a delegated
    /// child) keeps the private ledger its builder created. That fallback is
    /// deliberate — such a run must still be able to record intervals — but its
    /// checkpoints are not the authoritative ones: only the runtime-built agent's
    /// ledger is the one the interaction service also writes to.
    pub(crate) fn set_timing_ledger(&mut self, timing: SharedTimingLedger) {
        self.timing = timing;
    }

    /// Re-anchor the run clock at the start of a task run.
    pub(crate) fn begin_timing_run(&self) {
        self.timing.begin_run();
    }

    /// Close anything still open and return this run's increments since the
    /// previous checkpoint.
    ///
    /// Cancellation and failure paths reach a checkpoint with intervals still
    /// open. Closing them here is what makes "every terminal task accounts for
    /// its elapsed interval" true instead of nearly true, and closing is
    /// idempotent, so a second checkpoint cannot count the same interval twice.
    pub(crate) fn take_timing_checkpoint(&self) -> TimingCheckpoint {
        self.timing.finish();
        self.timing.take_checkpoint()
    }

    /// Restore the durable session time this process starts from.
    ///
    /// Called once, when a session is restored, for the same reason the usage
    /// counters are restored there: a budget that only counted this process
    /// would hand a resumed session a fresh allowance.
    pub(crate) fn restore_session_time(&mut self, active_compute_ms: u64, active_run_ms: u64) {
        self.restored_session_time = RestoredSessionTime {
            active_compute_ms,
            active_run_ms,
        };
    }

    /// The clock the session active-time budget consumes, and what it has spent.
    ///
    /// One code path for both policies: the restored base is chosen by the
    /// ledger's policy, and the consumed reading is that base plus whatever the
    /// ledger has closed since the run re-anchored. Only closed intervals
    /// contribute, so a reading taken mid-interval lags real spending by at most
    /// one open interval — the per-run wall deadline is the backstop.
    pub(crate) fn session_time_reading(&self) -> SessionTimeReading {
        let snapshot = self.timing.snapshot();
        let restored = self.restored_session_time;
        let run_compute_ms = u64::try_from(snapshot.active_compute_ms).unwrap_or(0);
        let run_waiting_ms = u64::try_from(snapshot.waiting_ms).unwrap_or(0);
        let consumed_ms = u64::try_from(snapshot.budget_consumed_ms()).unwrap_or(0);
        let restored_base_ms = match snapshot.policy {
            TimingPolicy::ActiveCompute => restored.active_compute_ms,
            TimingPolicy::WallClock => restored.active_run_ms,
        };
        SessionTimeReading {
            policy: snapshot.policy,
            used_ms: restored_base_ms.saturating_add(consumed_ms),
            active_compute_ms: restored.active_compute_ms.saturating_add(run_compute_ms),
            // Waiting has no durable counter of its own: it is what the durable
            // wall union holds beyond the compute the same history recorded.
            waiting_ms: restored
                .active_run_ms
                .saturating_sub(restored.active_compute_ms)
                .saturating_add(run_waiting_ms),
        }
    }
}
