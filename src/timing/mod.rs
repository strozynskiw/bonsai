//! Typed runtime intervals for one agent run.
//!
//! A run's wall-clock time mixes work with waiting: model execution, active
//! tool processes, build queues, permission prompts, `question` waits, and
//! child agents. Reporting that single number as "active work" makes a run that
//! spent most of its time waiting on the user look expensive, and it lets queue
//! time hide inside tool failures.
//!
//! [`TimingLedger`] records each interval with its category, lane, owner, and —
//! for waits — a reason. Durations are derived from a monotonic clock so a wall
//! clock adjustment cannot corrupt them, while every interval also carries
//! wall-clock anchors for persistence and audit.
//!
//! Overlap is explicit, not accidental: parallel tool calls and concurrent
//! children each record their own interval, so a summary reports both the
//! *critical path* (the union of all intervals — what the run actually took)
//! and *summed resource time* per category (what the category would have cost
//! serially, which may exceed the critical path).
//!
//! Work and waiting are separated, not only measured: a summary reports the
//! active-compute clock (model, active tool, and child-active time) separately
//! from waiting (queue, permission, user, and child-wait time), and
//! [`TimingPolicy`] decides which clock a run's budgets consume. The policy is
//! persisted and reported per task run, and it drives the cumulative session
//! active-time budget: `ActiveCompute` charges closed work intervals that
//! [`TimingLedger::snapshot`] has recorded, `WallClock` charges elapsed run
//! time. `max_run_duration` stays an independent per-run wall deadline under
//! both, so excluding waits can never let a run continue unbounded.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(crate) const MAX_OWNER_CHARS: usize = 200;
pub(crate) const MAX_REASON_CHARS: usize = 512;

/// Monotonic and wall-clock time source.
///
/// Duration math must use [`Clock::monotonic_ms`]; it never decreases, so a
/// system clock adjustment or an NTP step cannot produce a negative or inflated
/// interval. [`Clock::wall_ms`] exists only for persisted anchors and audit.
pub(crate) trait Clock: fmt::Debug + Send + Sync {
    /// Milliseconds since a process-local origin. Never decreases.
    fn monotonic_ms(&self) -> i64;

    /// Milliseconds since the Unix epoch.
    fn wall_ms(&self) -> i64;
}

/// Production clock: monotonic elapsed time plus the system wall clock.
#[derive(Debug)]
pub(crate) struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic_ms(&self) -> i64 {
        i64::try_from(self.origin.elapsed().as_millis()).unwrap_or(i64::MAX)
    }

    fn wall_ms(&self) -> i64 {
        crate::util::time::now_ms()
    }
}

/// Deterministic clock for tests: both readings advance only when the test says
/// so. The monotonic reading saturates at zero, so it can never be made to move
/// backwards — the property duration math relies on.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeClock {
    state: Mutex<FakeClockState>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct FakeClockState {
    monotonic_ms: i64,
    wall_ms: i64,
}

#[cfg(test)]
impl FakeClock {
    /// Start at the given wall-clock instant with a zeroed monotonic reading.
    pub(crate) fn new(wall_ms: i64) -> Self {
        Self {
            state: Mutex::new(FakeClockState {
                monotonic_ms: 0,
                wall_ms,
            }),
        }
    }

    /// Advance both readings by `ms`, saturating instead of going backwards.
    /// Returns the new monotonic reading.
    pub(crate) fn advance_ms(&self, ms: i64) -> i64 {
        let mut state = self.lock();
        state.monotonic_ms = state.monotonic_ms.saturating_add(ms.max(0));
        state.wall_ms = state.wall_ms.saturating_add(ms);
        state.monotonic_ms
    }

    /// Move the wall clock alone, modelling an adjustment mid-run.
    pub(crate) fn set_wall_ms(&self, wall_ms: i64) {
        self.lock().wall_ms = wall_ms;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeClockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn monotonic_ms(&self) -> i64 {
        self.lock().monotonic_ms
    }

    fn wall_ms(&self) -> i64 {
        self.lock().wall_ms
    }
}

/// What the run was doing during an interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum TimingCategory {
    /// Provider request and response streaming, retries included.
    ModelRequest,
    /// An active tool process, local or remote.
    ToolActive,
    /// Waiting in a scheduler or build queue before work could start.
    Queue,
    /// Waiting for a permission or approval decision.
    PermissionWait,
    /// Waiting for a user answer (`question`).
    UserWait,
    /// A child agent actively working.
    ChildActive,
    /// Waiting for a child agent to finish.
    ChildWait,
}

impl TimingCategory {
    /// Every category, in report order.
    pub(crate) const ALL: [Self; 7] = [
        Self::ModelRequest,
        Self::ToolActive,
        Self::Queue,
        Self::PermissionWait,
        Self::UserWait,
        Self::ChildActive,
        Self::ChildWait,
    ];

    /// Persisted wire value.
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::ModelRequest => "model_request",
            Self::ToolActive => "tool_active",
            Self::Queue => "queue",
            Self::PermissionWait => "permission_wait",
            Self::UserWait => "user_wait",
            Self::ChildActive => "child_active",
            Self::ChildWait => "child_wait",
        }
    }

    /// Decode a persisted wire value.
    ///
    /// Unknown values are dropped rather than folded into a known category: an
    /// unrecognised category must not be silently reported as model or tool
    /// work, which is exactly the confusion this module exists to remove.
    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_db_str() == value)
    }

    /// Short human label for reports.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ModelRequest => "model",
            Self::ToolActive => "tools",
            Self::Queue => "queue",
            Self::PermissionWait => "permission wait",
            Self::UserWait => "user wait",
            Self::ChildActive => "child active",
            Self::ChildWait => "child wait",
        }
    }

    /// Whether the category is work that consumes the default active-compute
    /// budget.
    pub(crate) const fn consumes_active_compute(self) -> bool {
        matches!(
            self,
            Self::ModelRequest | Self::ToolActive | Self::ChildActive
        )
    }

    /// Whether the category is waiting rather than working.
    pub(crate) const fn is_wait(self) -> bool {
        matches!(
            self,
            Self::Queue | Self::PermissionWait | Self::UserWait | Self::ChildWait
        )
    }
}

/// Which agent produced an interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum TimingLane {
    Parent,
    Child,
}

impl TimingLane {
    pub(crate) const ALL: [Self; 2] = [Self::Parent, Self::Child];

    /// Persisted wire value.
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Child => "child",
        }
    }

    /// Decode a persisted wire value.
    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|lane| lane.as_db_str() == value)
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Child => "child",
        }
    }
}

/// Which clock a run's budgets and deadlines consume.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TimingPolicy {
    /// Default. Budgets consume model, active tool, and child-active time, so
    /// permission prompts, user questions, and queue waits do not exhaust them.
    #[default]
    ActiveCompute,
    /// Explicit opt-in for wall-clock deadlines: the whole run elapsed time,
    /// waits included, counts against the limit.
    WallClock,
}

impl TimingPolicy {
    pub(crate) const ALL: [Self; 2] = [Self::ActiveCompute, Self::WallClock];

    /// Persisted wire value.
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::ActiveCompute => "active_compute",
            Self::WallClock => "wall_clock",
        }
    }

    /// Decode a persisted wire value.
    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|policy| policy.as_db_str() == value)
    }

    /// Short human label for reports.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ActiveCompute => "active compute",
            Self::WallClock => "wall clock",
        }
    }

    /// Environment variable that selects the policy recorded for new runs.
    pub(crate) const ENV_VAR: &'static str = "BONSAI_TIMING_POLICY";

    /// Policy for a new run: the explicit opt-in from the environment, or the
    /// default active-compute policy.
    ///
    /// The value is persisted with the run's timing, shown in reports, and it
    /// **decides what stops a run**: the cumulative session active-time budget
    /// consumes this clock, and no session wall deadline is armed under
    /// active compute. `max_run_duration` remains an independent per-run wall
    /// deadline under both policies.
    ///
    /// An unset or unreadable value keeps the default on purpose: a typo in an
    /// opt-in must never silently start charging a run for the time it spent
    /// waiting on its user.
    pub(crate) fn from_env() -> Self {
        std::env::var(Self::ENV_VAR)
            .ok()
            .and_then(|value| Self::from_db_str(value.trim()))
            .unwrap_or_default()
    }
}

/// Opaque handle for one open interval.
///
/// Handles are `Copy` so a caller can hold one across an `await` without
/// borrowing the ledger, and closing one twice is a no-op rather than a
/// double count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TimingSpan(u64);

/// One closed interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimingInterval {
    pub(crate) category: TimingCategory,
    pub(crate) lane: TimingLane,
    pub(crate) owner: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) started_monotonic_ms: i64,
    pub(crate) ended_monotonic_ms: i64,
    pub(crate) started_wall_ms: i64,
    pub(crate) ended_wall_ms: i64,
}

impl TimingInterval {
    /// Duration from the monotonic clock, never negative.
    pub(crate) fn duration_ms(&self) -> i64 {
        (self.ended_monotonic_ms - self.started_monotonic_ms).max(0)
    }

    /// Monotonic `(start, end)` pair, for union arithmetic.
    pub(crate) const fn range(&self) -> (i64, i64) {
        (self.started_monotonic_ms, self.ended_monotonic_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenInterval {
    category: TimingCategory,
    lane: TimingLane,
    owner: Option<String>,
    reason: Option<String>,
    started_monotonic_ms: i64,
    started_wall_ms: i64,
}

/// Per-category, per-lane aggregate of a run's intervals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CategoryTiming {
    pub(crate) category: TimingCategory,
    pub(crate) lane: TimingLane,
    /// Summed resource time. Concurrent intervals each contribute in full, so
    /// this may exceed the critical path.
    pub(crate) summed_ms: i64,
    pub(crate) interval_count: u32,
    /// Intervals still open when the summary was taken. A recovered run clears
    /// these without inventing a duration.
    pub(crate) open_count: u32,
    /// Most recent owner label (tool name, child lane id), bounded and redacted.
    pub(crate) latest_owner: Option<String>,
    /// Most recent wait reason, bounded and redacted.
    pub(crate) latest_reason: Option<String>,
}

/// One run's timing as of a single instant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TimingSnapshot {
    pub(crate) policy: TimingPolicy,
    /// Run elapsed time on the monotonic clock.
    pub(crate) elapsed_ms: i64,
    /// Union of every interval: what the run actually took, without double
    /// counting concurrent work.
    pub(crate) critical_path_ms: i64,
    /// Union of model, active tool, and child-active intervals.
    pub(crate) active_compute_ms: i64,
    /// Union of queue, permission, user, and child-wait intervals.
    pub(crate) waiting_ms: i64,
    pub(crate) categories: Vec<CategoryTiming>,
    /// Intervals open at snapshot time, summed across categories.
    pub(crate) open_interval_count: u32,
}

impl TimingSnapshot {
    /// Summed resource time in one category and lane, for tests and reports.
    #[cfg(test)]
    pub(crate) fn category_summed_ms(&self, category: TimingCategory, lane: TimingLane) -> i64 {
        self.categories
            .iter()
            .find(|row| row.category == category && row.lane == lane)
            .map_or(0, |row| row.summed_ms)
    }

    /// The clock reading a budget or deadline consumes under this policy.
    ///
    /// Only closed intervals contribute: an interval still open (say a long
    /// provider stream) is charged when it closes, so a reading taken mid-turn
    /// lags real spending by at most one open interval.
    pub(crate) const fn budget_consumed_ms(&self) -> i64 {
        match self.policy {
            TimingPolicy::ActiveCompute => self.active_compute_ms,
            TimingPolicy::WallClock => self.elapsed_ms,
        }
    }
}

/// One category's contribution recorded since the previous checkpoint.
///
/// `summed_ms` and `interval_count` are increments a caller *adds* to persisted
/// totals; `open_count` is the live number of open intervals in the category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CategoryTimingDelta {
    pub(crate) category: TimingCategory,
    pub(crate) lane: TimingLane,
    pub(crate) summed_ms: i64,
    pub(crate) interval_count: u32,
    pub(crate) open_count: u32,
    pub(crate) latest_owner: Option<String>,
    pub(crate) latest_reason: Option<String>,
}

/// The increments one checkpoint adds to a task run's persisted timing.
///
/// Checkpoints are incremental on purpose: a run can be resumed in a new
/// process after a crash, and a resumed segment must add to what the interrupted
/// segment already recorded instead of overwriting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimingCheckpoint {
    pub(crate) policy: TimingPolicy,
    /// Run clock elapsed in this window.
    pub(crate) wall_ms: i64,
    pub(crate) critical_path_ms: i64,
    pub(crate) active_compute_ms: i64,
    pub(crate) waiting_ms: i64,
    pub(crate) categories: Vec<CategoryTimingDelta>,
    pub(crate) open_interval_count: u32,
}

impl TimingCheckpoint {
    /// Whether the window recorded nothing at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.categories.is_empty() && self.open_interval_count == 0 && self.wall_ms == 0
    }
}

/// Records one run's intervals.
///
/// Cheap to share: every method takes `&self` and the state is held behind a
/// mutex, so a tool execution context can record a wait that the run loop
/// cannot see while the run loop records its own intervals.
#[derive(Debug)]
pub(crate) struct TimingLedger {
    clock: Arc<dyn Clock>,
    policy: TimingPolicy,
    state: Mutex<LedgerState>,
}

#[derive(Debug)]
struct LedgerState {
    started_monotonic_ms: i64,
    started_wall_ms: i64,
    /// Start of the not-yet-persisted checkpoint window.
    checkpoint_monotonic_ms: i64,
    next_span_id: u64,
    open: BTreeMap<u64, OpenInterval>,
    intervals: Vec<TimingInterval>,
}

impl TimingLedger {
    /// Start a run ledger. The run's elapsed clock starts now.
    pub(crate) fn new(clock: Arc<dyn Clock>, policy: TimingPolicy) -> Self {
        let started_monotonic_ms = clock.monotonic_ms();
        let started_wall_ms = clock.wall_ms();
        Self {
            clock,
            policy,
            state: Mutex::new(LedgerState {
                started_monotonic_ms,
                started_wall_ms,
                checkpoint_monotonic_ms: started_monotonic_ms,
                next_span_id: 1,
                open: BTreeMap::new(),
                intervals: Vec::new(),
            }),
        }
    }

    /// Start a run ledger with the production clock.
    pub(crate) fn system(policy: TimingPolicy) -> Self {
        Self::new(Arc::new(SystemClock::new()), policy)
    }

    /// Re-anchor the run clock and start a new checkpoint window.
    ///
    /// One agent serves a whole session, so the ledger outlives a single run:
    /// the run clock is re-anchored here, at the start of a task run, rather
    /// than at ledger construction. Intervals recorded by the previous run have
    /// already been checkpointed into that run's rows by then, so clearing them
    /// cannot lose evidence — it only keeps one run's intervals from being
    /// reported under the next run.
    pub(crate) fn begin_run(&self) {
        let now_monotonic_ms = self.clock.monotonic_ms();
        let started_wall_ms = self.clock.wall_ms();
        let mut state = self.lock();
        state.started_monotonic_ms = now_monotonic_ms;
        state.started_wall_ms = started_wall_ms;
        state.checkpoint_monotonic_ms = now_monotonic_ms;
        state.intervals.clear();
        state.open.clear();
    }

    /// Open one interval. `owner` and `reason` are placeholders for a tool,
    /// child lane, or wait cause; both are bounded and redacted before they are
    /// persisted.
    pub(crate) fn begin(
        &self,
        category: TimingCategory,
        lane: TimingLane,
        owner: Option<&str>,
        reason: Option<&str>,
    ) -> TimingSpan {
        let mut state = self.lock();
        let id = state.next_span_id;
        state.next_span_id = id.saturating_add(1);
        state.open.insert(
            id,
            OpenInterval {
                category,
                lane,
                owner: owner.map(|value| bounded_redacted(value, MAX_OWNER_CHARS)),
                reason: reason.map(|value| bounded_redacted(value, MAX_REASON_CHARS)),
                started_monotonic_ms: self.clock.monotonic_ms(),
                started_wall_ms: self.clock.wall_ms(),
            },
        );
        TimingSpan(id)
    }

    /// Close one interval. Returns the recorded duration, or `None` when the
    /// span was already closed — closing twice must never double count.
    pub(crate) fn end(&self, span: TimingSpan) -> Option<i64> {
        let mut state = self.lock();
        let open = state.open.remove(&span.0)?;
        let ended_monotonic_ms = self.clock.monotonic_ms().max(open.started_monotonic_ms);
        let ended_wall_ms = self.clock.wall_ms();
        let interval = TimingInterval {
            category: open.category,
            lane: open.lane,
            owner: open.owner,
            reason: open.reason,
            started_monotonic_ms: open.started_monotonic_ms,
            ended_monotonic_ms,
            started_wall_ms: open.started_wall_ms,
            ended_wall_ms,
        };
        let duration = interval.duration_ms();
        state.intervals.push(interval);
        Some(duration)
    }

    /// Close every open interval at the current instant and summarize.
    ///
    /// Used by the run teardown path, including cancellation, so a terminal run
    /// never leaves an interval open. Calling it twice is safe: the second call
    /// has nothing left to close and returns the same intervals.
    pub(crate) fn finish(&self) -> TimingSnapshot {
        let open: Vec<u64> = self.lock().open.keys().copied().collect();
        for span in open {
            self.end(TimingSpan(span));
        }
        self.snapshot()
    }

    /// Close every open interval without recording time for them.
    ///
    /// Restart recovery cannot know how long an interrupted interval ran before
    /// the process died, and inventing a duration would corrupt the accounting.
    /// Recovered intervals are therefore dropped and reported through the
    /// caller's recovered-interval counter, so a budget undercounts rather than
    /// inventing work that may not have happened. Returns how many intervals
    /// were dropped.
    #[cfg(test)]
    pub(crate) fn abandon_open(&self) -> u32 {
        let mut state = self.lock();
        let dropped = u32::try_from(state.open.len()).unwrap_or(u32::MAX);
        state.open.clear();
        dropped
    }

    /// Summarize the run as of now.
    ///
    /// Only closed intervals contribute time. An interval that is still open is
    /// reported as `open_count` on its category, so a reader can see that work
    /// is in flight while the totals stay free of time that a later `end` would
    /// count a second time.
    pub(crate) fn snapshot(&self) -> TimingSnapshot {
        let now_monotonic_ms = self.clock.monotonic_ms();
        let state = self.lock();
        let elapsed_ms = (now_monotonic_ms - state.started_monotonic_ms).max(0);
        let intervals = &state.intervals;
        let critical_path_ms = union_ms(intervals.iter().map(TimingInterval::range));
        let work_ranges: Vec<(i64, i64)> = intervals
            .iter()
            .filter(|interval| interval.category.consumes_active_compute())
            .map(TimingInterval::range)
            .collect();
        let wait_ranges: Vec<(i64, i64)> = intervals
            .iter()
            .filter(|interval| interval.category.is_wait())
            .map(TimingInterval::range)
            .collect();
        // Waiting that happened *inside* a work interval — a permission prompt
        // during a tool call, a queue wait inside the tasks tool — is still
        // waiting. Charging it to active compute would make "waiting is excluded
        // from the compute clock" false, so the work union has its overlap with
        // the wait union removed.
        let waiting_ms = union_ms(wait_ranges.iter().copied());
        let active_compute_ms = union_ms(work_ranges.iter().copied())
            .saturating_sub(intersection_ms(&work_ranges, &wait_ranges));

        let mut categories: Vec<CategoryTiming> = Vec::new();
        for interval in intervals {
            let row = category_row(&mut categories, interval.category, interval.lane);
            row.summed_ms = row.summed_ms.saturating_add(interval.duration_ms());
            row.interval_count = row.interval_count.saturating_add(1);
            if interval.owner.is_some() {
                row.latest_owner.clone_from(&interval.owner);
            }
            if interval.reason.is_some() {
                row.latest_reason.clone_from(&interval.reason);
            }
        }
        for open in state.open.values() {
            let row = category_row(&mut categories, open.category, open.lane);
            row.open_count = row.open_count.saturating_add(1);
            if open.owner.is_some() {
                row.latest_owner.clone_from(&open.owner);
            }
            if open.reason.is_some() {
                row.latest_reason.clone_from(&open.reason);
            }
        }
        let open_interval_count = u32::try_from(state.open.len()).unwrap_or(u32::MAX);
        categories.sort_by_key(|row| (row.category, row.lane));

        TimingSnapshot {
            policy: self.policy,
            elapsed_ms,
            critical_path_ms,
            active_compute_ms,
            waiting_ms,
            categories,
            open_interval_count,
        }
    }

    /// Fold everything recorded since the previous checkpoint into an
    /// incremental payload and advance the checkpoint window to now.
    ///
    /// Deltas are measured against the previous window, so a caller adds them to
    /// the persisted totals: taking a second checkpoint with nothing new
    /// contributes zero, and an interval that spans two checkpoints is counted
    /// once, in the window where it ended. The window advances even when the
    /// caller's write fails, so a lost write loses at most one window of
    /// accounting — it never duplicates time.
    pub(crate) fn take_checkpoint(&self) -> TimingCheckpoint {
        let now_monotonic_ms = self.clock.monotonic_ms();
        let mut state = self.lock();
        let window_start = state.checkpoint_monotonic_ms;
        let window_end = now_monotonic_ms.max(window_start);
        let clamp = |interval: &TimingInterval| {
            (
                interval.started_monotonic_ms.max(window_start),
                interval.ended_monotonic_ms.min(window_end),
            )
        };

        let mut categories: Vec<CategoryTimingDelta> = Vec::new();
        for interval in &state.intervals {
            let (start, end) = clamp(interval);
            let counted = interval.ended_monotonic_ms > window_start
                && interval.ended_monotonic_ms <= window_end;
            if end <= start && !counted {
                continue;
            }
            let row = delta_row(&mut categories, interval.category, interval.lane);
            row.summed_ms = row.summed_ms.saturating_add((end - start).max(0));
            if counted {
                row.interval_count = row.interval_count.saturating_add(1);
            }
            if interval.owner.is_some() {
                row.latest_owner.clone_from(&interval.owner);
            }
            if interval.reason.is_some() {
                row.latest_reason.clone_from(&interval.reason);
            }
        }
        for open in state.open.values() {
            let row = delta_row(&mut categories, open.category, open.lane);
            row.open_count = row.open_count.saturating_add(1);
            if open.owner.is_some() {
                row.latest_owner.clone_from(&open.owner);
            }
            if open.reason.is_some() {
                row.latest_reason.clone_from(&open.reason);
            }
        }

        let critical_path_ms = union_ms(state.intervals.iter().map(clamp));
        // Waiting nested inside a work interval is removed from the work clock,
        // exactly as in `snapshot`: a permission prompt answered during a tool
        // call is still waiting, not work.
        let wait_ranges: Vec<(i64, i64)> = state
            .intervals
            .iter()
            .filter(|interval| interval.category.is_wait())
            .map(clamp)
            .collect();
        let work_ranges: Vec<(i64, i64)> = state
            .intervals
            .iter()
            .filter(|interval| interval.category.consumes_active_compute())
            .map(clamp)
            .collect();
        let waiting_ms = union_ms(wait_ranges.iter().copied());
        let active_compute_ms = union_ms(work_ranges.iter().copied())
            .saturating_sub(intersection_ms(&work_ranges, &wait_ranges));
        let open_interval_count = u32::try_from(state.open.len()).unwrap_or(u32::MAX);
        state.checkpoint_monotonic_ms = window_end;
        categories.sort_by_key(|row| (row.category, row.lane));

        TimingCheckpoint {
            policy: self.policy,
            wall_ms: window_end.saturating_sub(window_start),
            critical_path_ms,
            active_compute_ms,
            waiting_ms,
            categories,
            open_interval_count,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Shared handle to one run's ledger.
pub(crate) type SharedTimingLedger = Arc<TimingLedger>;

/// Find or create the aggregate row for one category and lane.
fn category_row(
    rows: &mut Vec<CategoryTiming>,
    category: TimingCategory,
    lane: TimingLane,
) -> &mut CategoryTiming {
    let index = match rows
        .iter()
        .position(|row| row.category == category && row.lane == lane)
    {
        Some(index) => index,
        None => {
            rows.push(CategoryTiming {
                category,
                lane,
                summed_ms: 0,
                interval_count: 0,
                open_count: 0,
                latest_owner: None,
                latest_reason: None,
            });
            rows.len() - 1
        }
    };
    &mut rows[index]
}

/// Find or create the incremental row for one category and lane.
fn delta_row(
    rows: &mut Vec<CategoryTimingDelta>,
    category: TimingCategory,
    lane: TimingLane,
) -> &mut CategoryTimingDelta {
    let index = match rows
        .iter()
        .position(|row| row.category == category && row.lane == lane)
    {
        Some(index) => index,
        None => {
            rows.push(CategoryTimingDelta {
                category,
                lane,
                summed_ms: 0,
                interval_count: 0,
                open_count: 0,
                latest_owner: None,
                latest_reason: None,
            });
            rows.len() - 1
        }
    };
    &mut rows[index]
}

/// Merge overlapping or touching intervals into a minimal covering set.
fn merged(ranges: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut sorted: Vec<(i64, i64)> = ranges
        .iter()
        .copied()
        .filter(|(start, end)| end > start)
        .collect();
    sorted.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(sorted.len());
    for (start, end) in sorted {
        match out.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// Total length of the union of `ranges`, ignoring empty ones.
fn union_ms(ranges: impl Iterator<Item = (i64, i64)>) -> i64 {
    let ranges: Vec<(i64, i64)> = ranges.collect();
    merged(&ranges)
        .into_iter()
        .map(|(start, end)| end - start)
        .fold(0_i64, i64::saturating_add)
}

/// Total length shared by two sets of intervals, each treated as a union.
///
/// Used to remove waiting from work: a wait nested inside a work interval is in
/// both unions, and the exclusion is only real if that overlap is measured.
fn intersection_ms(left: &[(i64, i64)], right: &[(i64, i64)]) -> i64 {
    let left = merged(left);
    let right = merged(right);
    let (mut i, mut j) = (0_usize, 0_usize);
    let mut total = 0_i64;
    while i < left.len() && j < right.len() {
        let (left_start, left_end) = left[i];
        let (right_start, right_end) = right[j];
        let start = left_start.max(right_start);
        let end = left_end.min(right_end);
        if end > start {
            total = total.saturating_add(end - start);
        }
        if left_end < right_end {
            i += 1;
        } else {
            j += 1;
        }
    }
    total
}

/// Collapse whitespace, redact secrets, and bound the length of a label that is
/// about to be recorded.
///
/// Deliberately separate from the storage layer's helper: the ledger has to
/// bound and redact whatever it retains in memory whether or not a persistence
/// layer ever sees it, so `timing` must not depend on `storage`. Both callers
/// use the limits declared at the top of this module.
fn bounded_redacted(value: &str, max_chars: usize) -> String {
    crate::redact::redact(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger_with(policy: TimingPolicy) -> (Arc<FakeClock>, TimingLedger) {
        let clock = Arc::new(FakeClock::new(1_700_000_000_000));
        let shared: Arc<dyn Clock> = clock.clone();
        let ledger = TimingLedger::new(shared, policy);
        (clock, ledger)
    }

    fn ledger() -> (Arc<FakeClock>, TimingLedger) {
        ledger_with(TimingPolicy::ActiveCompute)
    }

    #[test]
    fn a_wait_inside_a_tool_call_is_removed_from_active_compute() {
        let (clock, ledger) = ledger();
        let tool = ledger.begin(
            TimingCategory::ToolActive,
            TimingLane::Parent,
            Some("bash"),
            None,
        );
        clock.advance_ms(200);
        let prompt = ledger.begin(
            TimingCategory::PermissionWait,
            TimingLane::Parent,
            Some("permission"),
            Some("waiting for approval"),
        );
        clock.advance_ms(400);
        ledger.end(prompt);
        clock.advance_ms(400);
        ledger.end(tool);

        // One second of tool work, 400ms of which was an approval prompt: the
        // prompt is waiting, and "waiting is excluded from the compute clock"
        // has to survive the prompt happening *inside* the tool call.
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.critical_path_ms, 1_000);
        assert_eq!(snapshot.waiting_ms, 400);
        assert_eq!(snapshot.active_compute_ms, 600);
        assert_eq!(snapshot.budget_consumed_ms(), 600);
    }

    #[test]
    fn a_nested_wait_is_also_removed_from_the_checkpoint_delta() {
        let (clock, ledger) = ledger();
        let tool = ledger.begin(TimingCategory::ToolActive, TimingLane::Parent, None, None);
        clock.advance_ms(100);
        let queue = ledger.begin(
            TimingCategory::Queue,
            TimingLane::Parent,
            Some("tasks"),
            Some("waiting for background work"),
        );
        clock.advance_ms(300);
        ledger.end(queue);
        ledger.end(tool);

        let checkpoint = ledger.take_checkpoint();
        assert_eq!(checkpoint.critical_path_ms, 400);
        assert_eq!(checkpoint.waiting_ms, 300);
        assert_eq!(checkpoint.active_compute_ms, 100);
    }

    fn summed(snapshot: &TimingSnapshot, category: TimingCategory, lane: TimingLane) -> i64 {
        snapshot.category_summed_ms(category, lane)
    }

    fn delta(snapshot: &TimingCheckpoint, category: TimingCategory, lane: TimingLane) -> i64 {
        snapshot
            .categories
            .iter()
            .find(|row| row.category == category && row.lane == lane)
            .map_or(0, |row| row.summed_ms)
    }

    fn delta_count(snapshot: &TimingCheckpoint, category: TimingCategory, lane: TimingLane) -> u32 {
        snapshot
            .categories
            .iter()
            .find(|row| row.category == category && row.lane == lane)
            .map_or(0, |row| row.interval_count)
    }

    #[test]
    fn overlapping_tool_intervals_keep_one_wall_interval_but_sum_resource_time() {
        let (clock, ledger) = ledger();
        let first = ledger.begin(
            TimingCategory::ToolActive,
            TimingLane::Parent,
            Some("bash"),
            None,
        );
        clock.advance_ms(100);
        let second = ledger.begin(
            TimingCategory::ToolActive,
            TimingLane::Parent,
            Some("grep"),
            None,
        );
        clock.advance_ms(100);
        assert_eq!(ledger.end(first), Some(200));
        clock.advance_ms(100);
        assert_eq!(ledger.end(second), Some(200));

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.critical_path_ms, 300);
        assert_eq!(snapshot.active_compute_ms, 300);
        assert_eq!(
            summed(&snapshot, TimingCategory::ToolActive, TimingLane::Parent),
            400
        );
        assert_eq!(snapshot.elapsed_ms, 300);
        assert_eq!(snapshot.open_interval_count, 0);
    }

    #[test]
    fn user_and_permission_wait_are_excluded_from_the_default_budget() {
        let (clock, ledger) = ledger();
        let model = ledger.begin(
            TimingCategory::ModelRequest,
            TimingLane::Parent,
            Some("gpt"),
            None,
        );
        clock.advance_ms(1_000);
        ledger.end(model);
        let wait = ledger.begin(
            TimingCategory::UserWait,
            TimingLane::Parent,
            Some("question"),
            Some("awaiting the user's answer"),
        );
        clock.advance_ms(5_000);
        ledger.end(wait);

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.active_compute_ms, 1_000);
        assert_eq!(snapshot.waiting_ms, 5_000);
        assert_eq!(snapshot.critical_path_ms, 6_000);
        assert_eq!(snapshot.budget_consumed_ms(), 1_000);
        assert_eq!(snapshot.elapsed_ms, 6_000);
    }

    #[test]
    fn an_explicit_wall_clock_policy_consumes_the_waits_too() {
        let (clock, ledger) = ledger_with(TimingPolicy::WallClock);
        let model = ledger.begin(TimingCategory::ModelRequest, TimingLane::Parent, None, None);
        clock.advance_ms(1_000);
        ledger.end(model);
        let wait = ledger.begin(
            TimingCategory::PermissionWait,
            TimingLane::Parent,
            None,
            None,
        );
        clock.advance_ms(5_000);
        ledger.end(wait);

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.active_compute_ms, 1_000);
        assert_eq!(snapshot.policy, TimingPolicy::WallClock);
        assert_eq!(snapshot.budget_consumed_ms(), 6_000);
    }

    #[test]
    fn child_work_and_child_wait_stay_separate() {
        let (clock, ledger) = ledger();
        let active = ledger.begin(
            TimingCategory::ChildActive,
            TimingLane::Child,
            Some("lane-1"),
            None,
        );
        clock.advance_ms(200);
        ledger.end(active);
        let wait = ledger.begin(
            TimingCategory::ChildWait,
            TimingLane::Parent,
            Some("lane-1"),
            Some("waiting for child lane-1"),
        );
        clock.advance_ms(300);
        ledger.end(wait);

        let snapshot = ledger.snapshot();
        assert_eq!(
            summed(&snapshot, TimingCategory::ChildActive, TimingLane::Child),
            200
        );
        assert_eq!(
            summed(&snapshot, TimingCategory::ChildWait, TimingLane::Parent),
            300
        );
        assert_eq!(snapshot.active_compute_ms, 200);
        assert_eq!(snapshot.waiting_ms, 300);
    }

    #[test]
    fn closing_a_span_twice_never_double_counts() {
        let (clock, ledger) = ledger();
        let span = ledger.begin(TimingCategory::ToolActive, TimingLane::Parent, None, None);
        clock.advance_ms(250);

        assert_eq!(ledger.end(span), Some(250));
        assert_eq!(ledger.end(span), None);
        assert_eq!(
            summed(
                &ledger.snapshot(),
                TimingCategory::ToolActive,
                TimingLane::Parent
            ),
            250
        );
    }

    #[test]
    fn finishing_a_cancelled_run_closes_open_intervals_exactly_once() {
        let (clock, ledger) = ledger();
        let span = ledger.begin(TimingCategory::ToolActive, TimingLane::Parent, None, None);
        clock.advance_ms(400);

        let cancelled = ledger.finish();
        let replayed = ledger.finish();

        assert_eq!(
            summed(&cancelled, TimingCategory::ToolActive, TimingLane::Parent),
            400
        );
        assert_eq!(
            summed(&replayed, TimingCategory::ToolActive, TimingLane::Parent),
            400
        );
        assert_eq!(replayed.open_interval_count, 0);
        assert_eq!(ledger.end(span), None);
    }

    #[test]
    fn a_wall_clock_adjustment_cannot_corrupt_a_duration() {
        let (clock, ledger) = ledger();
        let span = ledger.begin(TimingCategory::ModelRequest, TimingLane::Parent, None, None);
        clock.set_wall_ms(1_700_003_600_000);
        clock.advance_ms(120);

        assert_eq!(ledger.end(span), Some(120));
        assert_eq!(
            summed(
                &ledger.snapshot(),
                TimingCategory::ModelRequest,
                TimingLane::Parent
            ),
            120
        );
    }

    #[test]
    fn checkpoints_are_incremental_and_repeat_as_zero() {
        let (clock, ledger) = ledger();
        let span = ledger.begin(TimingCategory::ToolActive, TimingLane::Parent, None, None);
        clock.advance_ms(100);
        ledger.end(span);

        let first = ledger.take_checkpoint();
        assert_eq!(first.critical_path_ms, 100);
        assert_eq!(first.active_compute_ms, 100);
        assert_eq!(
            delta(&first, TimingCategory::ToolActive, TimingLane::Parent),
            100
        );
        assert_eq!(
            delta_count(&first, TimingCategory::ToolActive, TimingLane::Parent),
            1
        );

        let repeated = ledger.take_checkpoint();
        assert!(repeated.is_empty());
        assert_eq!(repeated.critical_path_ms, 0);
        assert!(repeated.categories.is_empty());

        clock.advance_ms(50);
        let next = ledger.begin(TimingCategory::ModelRequest, TimingLane::Parent, None, None);
        clock.advance_ms(50);
        ledger.end(next);
        let third = ledger.take_checkpoint();
        assert_eq!(third.critical_path_ms, 50);
        assert_eq!(
            delta(&third, TimingCategory::ModelRequest, TimingLane::Parent),
            50
        );
        assert_eq!(
            delta_count(&third, TimingCategory::ModelRequest, TimingLane::Parent),
            1
        );
    }

    #[test]
    fn an_interval_spanning_two_checkpoints_is_counted_in_the_later_window() {
        let (clock, ledger) = ledger();
        let span = ledger.begin(TimingCategory::ToolActive, TimingLane::Parent, None, None);
        clock.advance_ms(100);

        let open_window = ledger.take_checkpoint();
        assert_eq!(open_window.critical_path_ms, 0);
        assert_eq!(open_window.open_interval_count, 1);
        assert_eq!(
            delta(&open_window, TimingCategory::ToolActive, TimingLane::Parent),
            0
        );

        clock.advance_ms(100);
        ledger.end(span);
        let closed_window = ledger.take_checkpoint();
        assert_eq!(closed_window.critical_path_ms, 100);
        assert_eq!(
            delta(
                &closed_window,
                TimingCategory::ToolActive,
                TimingLane::Parent
            ),
            100
        );
        assert_eq!(
            delta_count(
                &closed_window,
                TimingCategory::ToolActive,
                TimingLane::Parent
            ),
            1
        );
        assert_eq!(closed_window.open_interval_count, 0);
    }

    #[test]
    fn abandoned_intervals_contribute_no_time() {
        let (clock, ledger) = ledger();
        let _span = ledger.begin(TimingCategory::ModelRequest, TimingLane::Parent, None, None);
        clock.advance_ms(5_000);

        assert_eq!(ledger.abandon_open(), 1);
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.active_compute_ms, 0);
        assert_eq!(snapshot.critical_path_ms, 0);
        assert_eq!(snapshot.open_interval_count, 0);
        assert_eq!(ledger.abandon_open(), 0);
    }

    #[test]
    fn every_wire_value_round_trips_and_unknown_ones_are_dropped() {
        for category in TimingCategory::ALL {
            assert_eq!(
                TimingCategory::from_db_str(category.as_db_str()),
                Some(category)
            );
        }
        for lane in TimingLane::ALL {
            assert_eq!(TimingLane::from_db_str(lane.as_db_str()), Some(lane));
        }
        for policy in TimingPolicy::ALL {
            assert_eq!(TimingPolicy::from_db_str(policy.as_db_str()), Some(policy));
        }
        assert_eq!(TimingCategory::from_db_str("invented"), None);
        assert_eq!(TimingLane::from_db_str("grandchild"), None);
        assert_eq!(TimingPolicy::from_db_str("best_effort"), None);
    }

    #[test]
    fn owner_and_reason_labels_are_bounded_and_redacted() {
        let (clock, ledger) = ledger();
        let secret = format!("ghp_{}", "a".repeat(40));
        let long = format!("tool {secret} {}", "x".repeat(1_000));
        let span = ledger.begin(
            TimingCategory::PermissionWait,
            TimingLane::Parent,
            Some(&long),
            Some(&long),
        );
        clock.advance_ms(10);
        ledger.end(span);

        let snapshot = ledger.snapshot();
        let row = snapshot
            .categories
            .first()
            .expect("the wait recorded a category row");
        let owner = row.latest_owner.as_deref().expect("an owner label");
        let reason = row.latest_reason.as_deref().expect("a reason label");
        assert!(!owner.contains(&secret));
        assert!(!reason.contains(&secret));
        assert!(owner.chars().count() <= MAX_OWNER_CHARS);
        assert!(reason.chars().count() <= MAX_REASON_CHARS);
    }
}
