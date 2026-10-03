//! Live registry for nested subagents, the data behind the `/subagents`
//! view. Mirrors [`crate::background`] but for in-process `agent`-tool delegations
//! rather than OS background jobs. A [`RecordingSink`] feeds each running
//! subagent's activity into its record as it runs, and a broadcast channel lets
//! the TUI refresh the `/subagents` modal live — exactly like `/tasks`.
//!
//! The registry inner uses a *std* mutex (not tokio) because [`RecordingSink`]
//! writes from the synchronous [`OutputSink`] callbacks, which cannot `.await`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use tokio::sync::broadcast;

use crate::agent::{UsageTotals, UsageTurn};
use crate::model_catalog::{ConnectionId, ModelId};
use crate::output::OutputSink;
use crate::provider::ReasoningSelection;
use crate::tool::read_evidence::DelegatedReadEvidence;

/// Stable identity of a compiled, delegated subagent.
///
/// This is intentionally distinct from a custom-agent name and from an
/// [`crate::agent::ActivePersona`]: changing settings for one of these ids must
/// not turn the built-in into a user-defined, switchable persona.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BuiltinSubagentId {
    Explore,
    Review,
    SecurityReview,
    Research,
    /// The internal self-review policy lane. Unlike the others it is NOT a
    /// delegation target (absent from `BUILTIN_AGENTS`) — the model can never
    /// call it — but it IS a configurable-model identity: `/self-review model`
    /// stores its provider/model/effort here so the pre-done critique can run
    /// on a cheaper or independent model than the parent. Keep it out of the
    /// delegation catalog and out of the `/subagents` picker.
    SelfReview,
}

impl BuiltinSubagentId {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Review => "review",
            Self::SecurityReview => "security-review",
            Self::Research => "research",
            Self::SelfReview => "self-review",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "explore" => Some(Self::Explore),
            "review" => Some(Self::Review),
            "security-review" => Some(Self::SecurityReview),
            "research" => Some(Self::Research),
            "self-review" => Some(Self::SelfReview),
            _ => None,
        }
    }
}

impl std::fmt::Display for BuiltinSubagentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// User-owned settings layered over a compiled built-in subagent definition.
///
/// Model values use the same selector strings as custom-agent frontmatter: a
/// one-letter shortcut or a concrete `/model` selector. Missing values inherit
/// the parent model (or disable the fallback), while a missing registry entry
/// means the entire compiled definition is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltinSubagentSettings {
    pub(crate) enabled: bool,
    pub(crate) primary_model: Option<String>,
    pub(crate) primary_effort: Option<String>,
    pub(crate) fallback_model: Option<String>,
    pub(crate) fallback_effort: Option<String>,
}

impl Default for BuiltinSubagentSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            primary_model: None,
            primary_effort: None,
            fallback_model: None,
            fallback_effort: None,
        }
    }
}

pub(crate) type BuiltinSubagentSettingsRegistry =
    BTreeMap<BuiltinSubagentId, BuiltinSubagentSettings>;

/// Hot-swappable built-in settings shared by the runtime, resolver, and TUI.
#[derive(Debug, Clone, Default)]
pub(crate) struct SharedBuiltinSubagentSettings {
    inner: Arc<RwLock<BuiltinSubagentSettingsRegistry>>,
}

impl SharedBuiltinSubagentSettings {
    pub(crate) fn new(settings: BuiltinSubagentSettingsRegistry) -> Self {
        Self {
            inner: Arc::new(RwLock::new(settings)),
        }
    }

    pub(crate) fn snapshot(&self) -> BuiltinSubagentSettingsRegistry {
        self.inner
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(crate) fn get(&self, id: BuiltinSubagentId) -> Option<BuiltinSubagentSettings> {
        self.inner
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&id)
            .cloned()
    }

    #[cfg(test)]
    pub(crate) fn replace(&self, settings: BuiltinSubagentSettingsRegistry) {
        *self
            .inner
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = settings;
    }

    pub(crate) fn upsert(
        &self,
        id: BuiltinSubagentId,
        settings: BuiltinSubagentSettings,
    ) -> Option<BuiltinSubagentSettings> {
        self.inner
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id, settings)
    }

    #[cfg(test)]
    pub(crate) fn reset(&self, id: BuiltinSubagentId) -> Option<BuiltinSubagentSettings> {
        self.inner
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id)
    }
}

/// A per-agent provider/model/effort override. Frontmatter declarations leave
/// the provider fields empty so `model` can be resolved as a one-letter model
/// shortcut or `/model` selector; live `/subagents` picker overrides pin the
/// exact provider/catalog target selected by the user.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubagentModelOverride {
    pub provider_id: Option<String>,
    pub connection_id: Option<ConnectionId>,
    pub model_id: Option<ModelId>,
    pub model: String,
    pub reasoning: ReasoningSelection,
}

/// Ordered persisted assignment chain for one subagent. A live `/subagents`
/// override replaces this entire chain for the current session.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SubagentModelChain {
    pub primary: Option<SubagentModelOverride>,
    pub backup: Option<SubagentModelOverride>,
}

impl SubagentModelChain {
    pub(crate) fn single(primary: Option<SubagentModelOverride>) -> Self {
        Self {
            primary,
            backup: None,
        }
    }
}

impl SubagentModelOverride {
    pub(crate) fn selector(model: String, reasoning: ReasoningSelection) -> Self {
        Self {
            provider_id: None,
            connection_id: None,
            model_id: None,
            model,
            reasoning,
        }
    }

    pub(crate) fn explicit(
        provider_id: String,
        connection_id: Option<ConnectionId>,
        model_id: Option<ModelId>,
        model: String,
        reasoning: ReasoningSelection,
    ) -> Self {
        Self {
            provider_id: Some(provider_id),
            connection_id,
            model_id,
            model,
            reasoning,
        }
    }

    pub(crate) fn display_label(&self) -> String {
        let model = self
            .provider_id
            .as_deref()
            .map(|provider_id| provider_model_label(provider_id, &self.model))
            .unwrap_or_else(|| self.model.clone());
        format!("{} [{}]", model, self.reasoning.label())
    }
}

pub(crate) fn provider_model_label(provider_id: &str, model: &str) -> String {
    if model
        .split_once('/')
        .is_some_and(|(prefix, _)| prefix == provider_id)
    {
        model.to_string()
    } else {
        format!("{provider_id}:{model}")
    }
}

/// Session-scoped, in-memory per-agent provider/model overrides keyed by agent
/// name ("explore"/"review"/custom/"self-review"). Written by the `/subagents`
/// picker, read when a subagent's provider is minted. Not persisted; the base
/// (parent) provider/model is used for any agent without an entry.
pub(crate) type SubagentModelOverrides = Arc<Mutex<HashMap<String, SubagentModelOverride>>>;

const SUBTASK_ID_PREFIX: &str = "sub-";
/// Cap a subagent's running activity transcript so a chatty run can't grow it
/// without bound; the front is trimmed when it exceeds this.
const ACTIVITY_CAP_CHARS: usize = 8_000;
/// Cap the stored prompt/result so the snapshot stays small.
const PROMPT_CAP_CHARS: usize = 2_000;
const RESULT_CAP_CHARS: usize = 8_000;
const PARTIAL_RESULT_CAP_CHARS: usize = 4_000;
const ACTIVITY_TOOL_RESULT_CAP_CHARS: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

impl SubagentStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed out",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_finished(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One recorded subagent run. Internal, owns live state mutated by the sink.
#[derive(Debug, Clone)]
struct SubagentRecord {
    id: Arc<str>,
    agent: Arc<str>,
    prompt: Arc<str>,
    detached: bool,
    status: SubagentStatus,
    started_at: SystemTime,
    finished_at: Option<SystemTime>,
    activity: Arc<String>,
    result: Option<Arc<str>>,
    tool_call_id: Option<Arc<str>>,
    launch_group_id: Option<Arc<str>>,
    reported_to_agent: bool,
    usage: Option<UsageTotals>,
    usage_turns: Vec<UsageTurn>,
    delegated_read_evidence: Vec<DelegatedReadEvidence>,
    /// The model this run actually used (set once the provider is minted), so the
    /// `/subagents` view can show when an override took effect.
    model: Option<Arc<str>>,
}

impl SubagentRecord {
    fn snapshot(&self) -> SubagentSnapshot {
        SubagentSnapshot {
            id: self.id.clone(),
            agent: self.agent.clone(),
            prompt: self.prompt.clone(),
            detached: self.detached,
            status: self.status,
            started_at: self.started_at,
            finished_at: self.finished_at,
            activity: self.activity.clone(),
            result: self.result.clone(),
            model: self.model.clone(),
            tool_call_id: self.tool_call_id.clone(),
            launch_group_id: self.launch_group_id.clone(),
        }
    }
}

/// Public, clonable snapshot the UI consumes. `PartialEq` lets the reducer diff
/// it and keep the modal cursor stable across live refreshes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSnapshot {
    pub id: Arc<str>,
    pub agent: Arc<str>,
    pub prompt: Arc<str>,
    pub detached: bool,
    pub status: SubagentStatus,
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub activity: Arc<String>,
    pub result: Option<Arc<str>>,
    /// The model this run used, once its provider was minted (`None` until then).
    pub model: Option<Arc<str>>,
    /// The `agent` tool call that launched this run, so the TUI can surface
    /// run details (e.g. the model) on that call's detail view.
    pub tool_call_id: Option<Arc<str>>,
    /// Parent launch fan-out this run belongs to, once its tool result is attached.
    pub launch_group_id: Option<Arc<str>>,
}

impl SubagentSnapshot {
    /// Multi-line detail for the selected subagent.
    pub fn detail(&self) -> String {
        let mut detail = String::new();
        detail.push_str(&format!("ID: {}\n", self.id));
        detail.push_str(&format!("Agent: {}\n", self.agent));
        if self.detached {
            detail.push_str("Mode: background\n");
        }
        if let Some(model) = &self.model {
            detail.push_str(&format!("Model: {model}\n"));
        }
        if let Some(group) = &self.launch_group_id {
            detail.push_str(&format!("Launch group: {group}\n"));
        }
        detail.push_str(&format!("Status: {}\n", self.status.label()));
        detail.push_str(&format!("Elapsed: {}\n", self.duration_label()));
        detail.push_str(&format!("\nPrompt:\n{}\n", self.prompt));
        detail.push_str("\nActivity:\n");
        if self.activity.trim().is_empty() {
            detail.push_str("(no activity yet)\n");
        } else {
            detail.push_str(self.activity.trim_end());
            detail.push('\n');
        }
        if let Some(result) = &self.result {
            detail.push_str("\nResult:\n");
            detail.push_str(result);
            detail.push('\n');
        }
        detail
    }

    pub fn duration_label(&self) -> String {
        let end = self.finished_at.unwrap_or_else(SystemTime::now);
        let secs = end
            .duration_since(self.started_at)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if secs >= 60 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        }
    }

    /// Salvage a bounded, explicitly untrusted activity tail when a run cannot
    /// produce its final synthesis. This gives the parent concrete leads from
    /// already-paid work without presenting the partial transcript as a
    /// verified conclusion.
    pub(crate) fn partial_result(&self) -> Option<String> {
        let mut activity = self.activity.trim().to_string();
        if activity.is_empty() {
            return None;
        }
        trim_front(&mut activity, PARTIAL_RESULT_CAP_CHARS);
        Some(format!(
            "Partial subagent evidence (no final conclusion; verify these leads):\n{}",
            crate::tool::wrap_untrusted_content("subagent:partial", &activity),
        ))
    }

    fn model_completion_report(&self) -> String {
        let mut report = format!(
            "ID: {}\nAgent: {}\nStatus: {}",
            self.id,
            self.agent,
            self.status.label()
        );
        if let Some(model) = &self.model {
            report.push_str(&format!("\nModel: {model}"));
        }
        if let Some(group) = &self.launch_group_id {
            report.push_str(&format!("\nLaunch group: {group}"));
        }
        if let Some(result) = self
            .result
            .as_deref()
            .filter(|result| !result.trim().is_empty())
        {
            report.push_str("\n\nResult:\n");
            report.push_str(result);
        }
        report
    }
}

/// A detached/background subagent completion claimed for agent context
/// injection, carrying both its user-visible snapshot and its model usage.
#[derive(Debug, Clone)]
pub(crate) struct SubagentCompletion {
    pub(crate) snapshot: SubagentSnapshot,
    pub(crate) usage: Option<UsageTotals>,
    pub(crate) usage_turns: Vec<UsageTurn>,
    pub(crate) delegated_read_evidence: Vec<DelegatedReadEvidence>,
}

/// A detached subagent terminal state linked back to the `agent` tool call that
/// launched it, used by the TUI to close the original tool card.
#[derive(Debug, Clone)]
pub(crate) struct SubagentToolCompletion {
    pub(crate) tool_call_id: String,
    pub(crate) snapshot: SubagentSnapshot,
}

/// Broadcast signal that some subagent changed; the TUI re-pulls `list()` on any
/// (the variant is informational — the consumer refreshes the whole list).
#[derive(Debug, Clone, Copy)]
pub enum SubagentEvent {
    Started,
    Activity,
    Finished,
}

#[derive(Debug, Default)]
struct SubagentInner {
    next_id: u64,
    // Keyed by the numeric sequence (not the `sub-N` string) so `list()` yields
    // runs in chronological order — a `BTreeMap<String, _>` would order them
    // lexicographically (`sub-1, sub-10, sub-2, …`) once ids reach two digits.
    subagents: BTreeMap<u64, SubagentRecord>,
}

#[derive(Debug)]
pub struct SubagentRegistry {
    inner: Mutex<SubagentInner>,
    events: broadcast::Sender<SubagentEvent>,
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentRegistry {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Mutex::new(SubagentInner {
                next_id: 1,
                subagents: BTreeMap::new(),
            }),
            events,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SubagentEvent> {
        self.events.subscribe()
    }

    /// Register a new running subagent and return its id (e.g. `sub-1`).
    pub(crate) fn register(&self, agent: &str, prompt: &str, detached: bool) -> String {
        let id = {
            let mut inner = self.lock();
            let seq = inner.next_id;
            let id = format!("{SUBTASK_ID_PREFIX}{seq}");
            inner.next_id = inner.next_id.saturating_add(1);
            inner.subagents.insert(
                seq,
                SubagentRecord {
                    id: Arc::from(id.clone()),
                    agent: Arc::from(agent),
                    prompt: Arc::from(compact(prompt, PROMPT_CAP_CHARS)),
                    detached,
                    status: SubagentStatus::Running,
                    started_at: SystemTime::now(),
                    finished_at: None,
                    activity: Arc::new(String::new()),
                    result: None,
                    tool_call_id: None,
                    launch_group_id: None,
                    reported_to_agent: false,
                    usage: None,
                    usage_turns: Vec::new(),
                    delegated_read_evidence: Vec::new(),
                    model: None,
                },
            );
            id
        };
        let _ = self.events.send(SubagentEvent::Started);
        id
    }

    /// Link a subagent record to the tool call that spawned it, outside any
    /// launch group (foreground runs; detached runs attach through
    /// [`Self::attach_tool_call_in_group`]). If the run already finished,
    /// re-emit `Finished` so the TUI wake path observes it.
    pub(crate) fn attach_tool_call(
        &self,
        id: &str,
        tool_call_id: impl Into<String>,
    ) -> Option<SubagentSnapshot> {
        self.attach_tool_call_in_group(id, tool_call_id, None)
    }

    pub(crate) fn attach_tool_call_in_group(
        &self,
        id: &str,
        tool_call_id: impl Into<String>,
        launch_group_id: Option<String>,
    ) -> Option<SubagentSnapshot> {
        let tool_call_id = tool_call_id.into();
        let snapshot = {
            let mut inner = self.lock();
            let seq = subtask_seq(id)?;
            let record = inner.subagents.get_mut(&seq)?;
            record.tool_call_id = Some(Arc::from(tool_call_id));
            record.launch_group_id = launch_group_id.as_deref().map(Arc::<str>::from);
            if let Some(parent_tool_call_id) = record.tool_call_id.as_deref() {
                for turn in &mut record.usage_turns {
                    turn.attach_parent_context(parent_tool_call_id, launch_group_id.as_deref());
                }
            }
            for evidence in &mut record.delegated_read_evidence {
                evidence.launch_group_id = launch_group_id.clone();
            }
            record.snapshot()
        };
        if snapshot.status.is_finished() {
            let _ = self.events.send(SubagentEvent::Finished);
        }
        Some(snapshot)
    }

    /// Cheap projection for the TUI: each run's launching `agent` tool call and
    /// the model the run uses, for runs where both are known. Unlike a full
    /// snapshot list this clones no activity/result payloads, so it is safe to
    /// call on every subagent event burst.
    pub(crate) fn tool_call_models(&self) -> Vec<(String, String)> {
        let inner = self.lock();
        inner
            .subagents
            .values()
            .filter_map(|record| {
                Some((
                    record.tool_call_id.as_ref()?.to_string(),
                    record.model.as_ref()?.to_string(),
                ))
            })
            .collect()
    }

    /// Record the model a run is using, once its provider has been minted.
    pub(crate) fn set_model(&self, id: &str, model: String) {
        {
            let mut inner = self.lock();
            let Some(seq) = subtask_seq(id) else {
                return;
            };
            let Some(record) = inner.subagents.get_mut(&seq) else {
                return;
            };
            record.model = Some(Arc::from(model));
        }
        let _ = self.events.send(SubagentEvent::Activity);
    }

    /// Append text to a subagent's activity transcript (front-trimmed at the cap).
    pub(crate) fn append_activity(&self, id: &str, text: &str) {
        {
            let mut inner = self.lock();
            let Some(seq) = subtask_seq(id) else {
                return;
            };
            let Some(record) = inner.subagents.get_mut(&seq) else {
                return;
            };
            let activity = Arc::make_mut(&mut record.activity);
            activity.push_str(text);
            trim_front(activity, ACTIVITY_CAP_CHARS);
        }
        let _ = self.events.send(SubagentEvent::Activity);
    }

    /// Mark a subagent finished with a terminal status and final text. Idempotent:
    /// a second call (e.g. the drop guard after an explicit finish) is ignored.
    pub(crate) fn finish(&self, id: &str, status: SubagentStatus, result: Option<String>) {
        self.finish_with_usage(id, status, result, None, Vec::new());
    }

    /// Mark a subagent finished and retain usage for detached completion
    /// delivery. Idempotent like [`Self::finish`].
    pub(crate) fn finish_with_usage(
        &self,
        id: &str,
        status: SubagentStatus,
        result: Option<String>,
        usage: Option<UsageTotals>,
        usage_turns: Vec<UsageTurn>,
    ) {
        {
            let mut inner = self.lock();
            let Some(seq) = subtask_seq(id) else {
                return;
            };
            let Some(record) = inner.subagents.get_mut(&seq) else {
                return;
            };
            if record.status.is_finished() {
                return;
            }
            record.status = status;
            record.finished_at = Some(SystemTime::now());
            record.usage = usage;
            record.usage_turns = usage_turns;
            if let Some(parent_tool_call_id) = record.tool_call_id.as_deref() {
                for turn in &mut record.usage_turns {
                    turn.attach_parent_context(
                        parent_tool_call_id,
                        record.launch_group_id.as_deref(),
                    );
                }
            }
            if let Some(result) = result {
                // Length-cap but keep newlines: the conclusion is shown in the
                // `/subagents` detail pane, where a flattened single line (what
                // `compact` would produce) is unreadable for structured output.
                record.result = Some(Arc::from(cap_chars(&result, RESULT_CAP_CHARS)));
            }
        }
        let _ = self.events.send(SubagentEvent::Finished);
    }

    pub(crate) fn set_delegated_read_evidence(
        &self,
        id: &str,
        evidence: Vec<DelegatedReadEvidence>,
    ) {
        let mut inner = self.lock();
        let Some(seq) = subtask_seq(id) else {
            return;
        };
        let Some(record) = inner.subagents.get_mut(&seq) else {
            return;
        };
        record.delegated_read_evidence = evidence
            .into_iter()
            .map(|mut evidence| {
                evidence.launch_group_id = record.launch_group_id.as_deref().map(str::to_string);
                evidence
            })
            .collect();
    }

    pub(crate) fn drain_completed_for_agent(&self) -> Vec<SubagentCompletion> {
        let mut inner = self.lock();
        let ready_groups = ready_launch_groups(&inner);
        inner
            .subagents
            .values_mut()
            .filter_map(|record| {
                if record.detached
                    && record.status.is_finished()
                    && !record.reported_to_agent
                    && !matches!(record.status, SubagentStatus::Cancelled)
                    && ready_groups.contains(launch_group_key(record))
                {
                    record.reported_to_agent = true;
                    Some(SubagentCompletion {
                        snapshot: record.snapshot(),
                        usage: record.usage,
                        usage_turns: record.usage_turns.clone(),
                        delegated_read_evidence: record.delegated_read_evidence.clone(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    pub(crate) fn pending_completed_for_agent(&self) -> Vec<SubagentCompletion> {
        let inner = self.lock();
        let ready_groups = ready_launch_groups(&inner);
        inner
            .subagents
            .values()
            .filter(|record| {
                record.detached
                    && record.status.is_finished()
                    && !record.reported_to_agent
                    && !matches!(record.status, SubagentStatus::Cancelled)
                    && ready_groups.contains(launch_group_key(record))
            })
            .map(|record| SubagentCompletion {
                snapshot: record.snapshot(),
                usage: record.usage,
                usage_turns: record.usage_turns.clone(),
                delegated_read_evidence: record.delegated_read_evidence.clone(),
            })
            .collect()
    }

    pub(crate) fn completed_tool_calls(&self) -> Vec<SubagentToolCompletion> {
        let inner = self.lock();
        inner
            .subagents
            .values()
            // Detached runs normally own their tool-card reconciliation. The
            // internal self-review lane is foreground but also emits a
            // synthetic `agent` card outside normal tool dispatch, so retain
            // the registry as its terminal-state backstop too.
            .filter(|record| {
                (record.detached || record.agent.as_ref() == "self-review")
                    && record.status.is_finished()
            })
            .filter_map(|record| {
                record
                    .tool_call_id
                    .as_ref()
                    .map(|tool_call_id| SubagentToolCompletion {
                        tool_call_id: tool_call_id.to_string(),
                        snapshot: record.snapshot(),
                    })
            })
            .collect()
    }

    pub(crate) async fn wait_for_subagent(
        &self,
        id: &str,
        max_wait: Duration,
    ) -> Option<SubagentSnapshot> {
        // Subscribe before the snapshot check so a finish/activity event racing
        // the check still wakes this waiter instead of waiting for the timeout.
        let mut receiver = self.subscribe();
        let snapshot = self.snapshot(id)?;
        if snapshot.status.is_finished() || max_wait.is_zero() {
            return Some(snapshot);
        }

        let _ = tokio::time::timeout(max_wait, async {
            loop {
                match receiver.recv().await {
                    Ok(SubagentEvent::Finished) => {
                        if self
                            .snapshot(id)
                            .is_some_and(|snapshot| snapshot.status.is_finished())
                        {
                            break;
                        }
                    }
                    Ok(SubagentEvent::Activity) | Ok(SubagentEvent::Started) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;

        self.snapshot(id)
    }

    pub(crate) async fn wait_for_any_detached(&self, max_wait: Duration) -> Vec<SubagentSnapshot> {
        let mut receiver = self.subscribe();
        if max_wait.is_zero()
            || self
                .list_detached()
                .iter()
                .all(|subtask| subtask.status.is_finished())
        {
            return self.list_detached();
        }

        let _ = tokio::time::timeout(max_wait, async {
            loop {
                match receiver.recv().await {
                    Ok(SubagentEvent::Activity) | Ok(SubagentEvent::Finished) => break,
                    Ok(SubagentEvent::Started) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
        self.list_detached()
    }

    pub(crate) fn agent_wake_ready(&self) -> bool {
        let inner = self.lock();
        !ready_launch_groups(&inner).is_empty()
    }

    /// Whether any subagent is still running. This authoritative liveness
    /// projection deliberately includes both foreground and detached runs.
    pub(crate) fn has_running(&self) -> bool {
        self.lock()
            .subagents
            .values()
            .any(|record| !record.status.is_finished())
    }

    /// Whether any detached subagent is still running. Cheap (no snapshot
    /// clone): the redraw loop calls this per drawn frame to keep the active
    /// cadence up while background subagents animate, even when the main agent
    /// is idle.
    pub(crate) fn has_active_detached(&self) -> bool {
        self.lock()
            .subagents
            .values()
            .any(|record| record.detached && !record.status.is_finished())
    }

    pub(crate) fn running_status_report(&self) -> Option<String> {
        let subtasks = {
            let inner = self.lock();
            inner
                .subagents
                .values()
                .filter(|record| record.detached && !record.status.is_finished())
                .map(SubagentRecord::snapshot)
                .collect::<Vec<_>>()
        };
        if subtasks.is_empty() {
            return None;
        }

        let mut report = format!(
            "[Background subagent status]\n{} detached subagent{} running.",
            subtasks.len(),
            if subtasks.len() == 1 { "" } else { "s" }
        );
        for subtask in subtasks {
            report.push_str(&format!("\n- {}: '{}' running", subtask.id, subtask.agent,));
            if let Some(model) = &subtask.model {
                report.push_str(&format!(" ({model})"));
            }
        }
        report.push_str(
            "\n\nTheir live activity stays in /subtasks. Do not duplicate their in-flight work; wait for the final reports.",
        );
        Some(report)
    }

    pub fn list(&self) -> Vec<SubagentSnapshot> {
        self.lock()
            .subagents
            .values()
            .map(SubagentRecord::snapshot)
            .collect()
    }

    pub(crate) fn snapshot(&self, id: &str) -> Option<SubagentSnapshot> {
        let seq = subtask_seq(id)?;
        self.lock()
            .subagents
            .get(&seq)
            .map(SubagentRecord::snapshot)
    }

    pub(crate) fn list_detached(&self) -> Vec<SubagentSnapshot> {
        self.lock()
            .subagents
            .values()
            .filter(|record| record.detached)
            .map(SubagentRecord::snapshot)
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SubagentInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn launch_group_key(record: &SubagentRecord) -> &str {
    record.launch_group_id.as_deref().unwrap_or(&record.id)
}

fn ready_launch_groups(inner: &SubagentInner) -> HashSet<String> {
    inner
        .subagents
        .values()
        .filter(|record| {
            record.detached
                && record.tool_call_id.is_some()
                && !record.reported_to_agent
                && record.status.is_finished()
                && !matches!(record.status, SubagentStatus::Cancelled)
        })
        .map(launch_group_key)
        .filter(|group| {
            !inner.subagents.values().any(|record| {
                record.detached
                    && launch_group_key(record) == *group
                    && !record.status.is_finished()
            })
        })
        .map(str::to_string)
        .collect()
}

pub(crate) fn format_subagent_list(subagents: &[SubagentSnapshot]) -> String {
    if subagents.is_empty() {
        return "No background subagents".to_string();
    }

    let mut lines = Vec::with_capacity(subagents.len() + 1);
    lines.push(format!(
        "{} background subagent{}",
        subagents.len(),
        if subagents.len() == 1 { "" } else { "s" }
    ));
    lines.extend(subagents.iter().map(|subagent| {
        let mut line = format!(
            "{} [{}] {} as '{}' (elapsed {})",
            subagent.id,
            subagent.status.label(),
            if subagent.detached {
                "background"
            } else {
                "foreground"
            },
            subagent.agent,
            subagent.duration_label()
        );
        if let Some(model) = &subagent.model {
            line.push_str(&format!(" model={model}"));
        }
        if let Some(result) = &subagent.result
            && !result.trim().is_empty()
        {
            line.push_str(&format!(" result={}", compact(result, 160)));
        }
        line
    }));
    lines.join("\n")
}

pub(crate) fn subagent_completion_message(completions: &[SubagentCompletion]) -> String {
    if let [completion] = completions {
        completion.snapshot.model_completion_report()
    } else {
        let mut message = format!("{} detached subagents finished.", completions.len());
        for completion in completions {
            message.push_str(&format!(
                "\n\n--- {} ---\n{}",
                completion.snapshot.id,
                completion.snapshot.model_completion_report()
            ));
        }
        message
    }
}

/// RAII guard held for the duration of a subagent run. If dropped while the
/// subagent is still `Running` (e.g. the parent cancelled and the tool future was
/// dropped before an explicit `finish`), it marks the entry `Cancelled`.
pub(crate) struct SubagentRunGuard {
    registry: Arc<SubagentRegistry>,
    id: String,
}

impl SubagentRunGuard {
    pub(crate) fn new(registry: Arc<SubagentRegistry>, id: String) -> Self {
        Self { registry, id }
    }

    pub(crate) fn finish_with_usage(
        &self,
        status: SubagentStatus,
        result: Option<String>,
        usage: Option<UsageTotals>,
        usage_turns: Vec<UsageTurn>,
    ) {
        self.registry
            .finish_with_usage(&self.id, status, result, usage, usage_turns);
    }
}

impl Drop for SubagentRunGuard {
    fn drop(&mut self) {
        self.registry
            .finish(&self.id, SubagentStatus::Cancelled, None);
    }
}

/// An [`OutputSink`] that records a subagent's live activity into its registry
/// entry instead of forwarding to a UI, so the `/subagents` view can show what a
/// subagent is doing while it runs.
pub(crate) struct RecordingSink {
    registry: Arc<SubagentRegistry>,
    id: String,
}

impl RecordingSink {
    pub(crate) fn new(registry: Arc<SubagentRegistry>, id: String) -> Self {
        Self { registry, id }
    }
}

impl OutputSink for RecordingSink {
    fn assistant_delta(&self, text: &str) {
        self.registry.append_activity(&self.id, text);
    }

    fn assistant_done(&self) {
        self.registry.append_activity(&self.id, "\n");
    }

    fn tool_started(&self, _id: &str, name: &str, arguments: &str) {
        self.registry.append_activity(
            &self.id,
            &format!("\n▸ {name} {}\n", compact(arguments, 120)),
        );
    }

    fn tool_finished(&self, _id: &str, result: &str, status: crate::output::ToolExecutionStatus) {
        let result = compact(result, ACTIVITY_TOOL_RESULT_CAP_CHARS);
        let mark = if result.is_empty() {
            format!("  ⤷ {}\n", status.label())
        } else {
            format!("  ⤷ {}: {result}\n", status.label())
        };
        self.registry.append_activity(&self.id, &mark);
    }

    fn status(&self, text: &str) {
        self.registry
            .append_activity(&self.id, &format!("· {text}\n"));
    }
}

/// Parse a subtask id (`sub-N`) back to its numeric sequence, the map key.
fn subtask_seq(id: &str) -> Option<u64> {
    id.strip_prefix(SUBTASK_ID_PREFIX)?.parse().ok()
}

/// Collapse whitespace-runs and cap a string to `max` chars with an ellipsis.
/// For single-line labels (tool args, the list-row prompt) — use [`cap_chars`]
/// when internal newlines must be preserved.
fn compact(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
}

/// Cap a string to `max` chars with an ellipsis, preserving its internal
/// newlines (unlike [`compact`], which collapses all whitespace to one line).
fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
}

/// Trim the front of `s` so it holds at most `max` chars (keep the latest tail).
fn trim_front(s: &mut String, max: usize) {
    // Byte length is an upper bound on char count, so when it's already within
    // the cap there's nothing to trim — skip the full O(n) char walk that would
    // otherwise run on every streamed chunk.
    if s.len() <= max {
        return;
    }
    let count = s.chars().count();
    if count <= max {
        return;
    }
    let skip = count - max;
    let trimmed: String = s.chars().skip(skip).collect();
    *s = trimmed;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_finish_lifecycle() {
        let reg = SubagentRegistry::new();
        let id = reg.register("explore", "find the thing", false);
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.agent.as_ref(), "explore");
        assert_eq!(snap.status, SubagentStatus::Running);
        assert!(snap.prompt.contains("find the thing"));

        reg.finish(
            &id,
            SubagentStatus::Succeeded,
            Some("found at a.rs:1".into()),
        );
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.status, SubagentStatus::Succeeded);
        assert_eq!(snap.result.as_deref(), Some("found at a.rs:1"));
        assert!(snap.finished_at.is_some());
    }

    #[test]
    fn snapshots_share_immutable_text_payloads() {
        let reg = SubagentRegistry::new();
        let id = reg.register("explore", "find the thing", false);
        reg.finish(&id, SubagentStatus::Succeeded, Some("found it".into()));

        let first = reg.snapshot(&id).unwrap();
        let second = reg.snapshot(&id).unwrap();

        assert!(Arc::ptr_eq(&first.id, &second.id));
        assert!(Arc::ptr_eq(&first.agent, &second.agent));
        assert!(Arc::ptr_eq(&first.prompt, &second.prompt));
        assert!(Arc::ptr_eq(&first.activity, &second.activity));
        assert!(Arc::ptr_eq(
            first.result.as_ref().unwrap(),
            second.result.as_ref().unwrap()
        ));
    }

    #[test]
    fn has_running_includes_foreground_and_detached_runs() {
        let reg = SubagentRegistry::new();
        let foreground = reg.register("explore", "foreground", false);
        let detached = reg.register("research", "detached", true);

        assert!(reg.has_running());
        assert!(reg.has_active_detached());

        reg.finish(&foreground, SubagentStatus::Succeeded, None);
        assert!(reg.has_running());
        assert!(reg.has_active_detached());

        reg.finish(&detached, SubagentStatus::Failed, None);
        assert!(!reg.has_running());
        assert!(!reg.has_active_detached());
    }

    #[test]
    fn finish_is_idempotent() {
        let reg = SubagentRegistry::new();
        let id = reg.register("explore", "p", false);
        reg.finish(&id, SubagentStatus::Succeeded, Some("first".into()));
        reg.finish(&id, SubagentStatus::Cancelled, Some("second".into()));
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.status, SubagentStatus::Succeeded);
        assert_eq!(snap.result.as_deref(), Some("first"));
    }

    #[test]
    fn run_guard_marks_cancelled_on_drop() {
        let reg = Arc::new(SubagentRegistry::new());
        let id = reg.register("explore", "p", false);
        {
            let _guard = SubagentRunGuard::new(reg.clone(), id.clone());
            // dropped without finish → cancelled
        }
        assert_eq!(reg.snapshot(&id).unwrap().status, SubagentStatus::Cancelled);
    }

    #[test]
    fn run_guard_noop_after_explicit_finish() {
        let reg = Arc::new(SubagentRegistry::new());
        let id = reg.register("explore", "p", false);
        {
            let guard = SubagentRunGuard::new(reg.clone(), id.clone());
            guard.finish_with_usage(
                SubagentStatus::Succeeded,
                Some("done".into()),
                None,
                Vec::new(),
            );
        }
        assert_eq!(reg.snapshot(&id).unwrap().status, SubagentStatus::Succeeded);
    }

    #[test]
    fn tool_call_models_lists_runs_with_both_call_and_model_known() {
        let reg = SubagentRegistry::new();
        let linked = reg.register("research", "p", false);
        let unlinked = reg.register("explore", "p", false);

        // Nothing to project until a run has both its tool call and model.
        assert!(reg.tool_call_models().is_empty());
        reg.attach_tool_call(&linked, "call-1").unwrap();
        assert!(reg.tool_call_models().is_empty());

        reg.set_model(&linked, "openrouter:glm-4.7".to_string());
        reg.set_model(&unlinked, "codex:gpt-5.6".to_string());
        assert_eq!(
            reg.tool_call_models(),
            vec![("call-1".to_string(), "openrouter:glm-4.7".to_string())],
        );

        reg.set_model(&linked, "codex:gpt-5.6".to_string());
        assert_eq!(
            reg.tool_call_models(),
            vec![("call-1".to_string(), "codex:gpt-5.6".to_string())],
            "runtime fallback must replace the launching call's effective model",
        );
    }

    #[test]
    fn detached_completion_wakes_and_drains_once() {
        let reg = SubagentRegistry::new();
        let id = reg.register("explore", "p", true);
        reg.attach_tool_call(&id, "call-1").unwrap();
        reg.finish_with_usage(
            &id,
            SubagentStatus::Succeeded,
            Some("done".into()),
            Some(UsageTotals {
                prompt_tokens: 3,
                completion_tokens: 2,
                cost_micros: Some(0),
                no_cache_cost_micros: Some(0),
                input_cache: None,
            }),
            Vec::new(),
        );

        assert!(reg.agent_wake_ready());
        let completed = reg.drain_completed_for_agent();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].snapshot.id.as_ref(), id);
        assert_eq!(completed[0].snapshot.result.as_deref(), Some("done"));
        assert!(!reg.agent_wake_ready());
        assert!(reg.drain_completed_for_agent().is_empty());
    }

    #[test]
    fn detached_completion_retains_child_evidence_with_its_subtask_id() {
        let reg = SubagentRegistry::new();
        let id = reg.register("explore", "p", true);
        reg.set_delegated_read_evidence(
            &id,
            vec![DelegatedReadEvidence {
                subtask_id: id.clone(),
                launch_group_id: None,
                source_id: "tool:child-read".to_string(),
                cited_in_result: true,
                evidence: crate::tool::ReadEvidence::new(
                    "src/lib.rs",
                    std::path::PathBuf::from("/tmp/src/lib.rs"),
                    crate::tool::ReadWindow {
                        requested_offset: 1,
                        requested_limit: 20,
                        start_line: 1,
                        end_line: Some(20),
                        total_lines: Some(20),
                    },
                    crate::tool::ReadCoverage::Full,
                    "1: content\n",
                    None,
                    0,
                    None,
                ),
            }],
        );
        reg.finish(&id, SubagentStatus::Succeeded, Some("src/lib.rs:1".into()));
        reg.attach_tool_call_in_group(&id, "call-1", Some("group-fast".to_string()))
            .unwrap();

        let completed = reg.drain_completed_for_agent();
        assert_eq!(completed[0].delegated_read_evidence.len(), 1);
        assert_eq!(completed[0].delegated_read_evidence[0].subtask_id, id);
        assert_eq!(
            completed[0].delegated_read_evidence[0]
                .launch_group_id
                .as_deref(),
            Some("group-fast")
        );
    }

    #[test]
    fn launch_group_waits_for_all_members_and_drains_one_terminal_report() {
        let reg = SubagentRegistry::new();
        let first = reg.register("explore", "first", true);
        let second = reg.register("explore", "second", true);
        reg.attach_tool_call_in_group(&first, "call-1", Some("group-9".to_string()))
            .unwrap();
        reg.attach_tool_call_in_group(&second, "call-2", Some("group-9".to_string()))
            .unwrap();

        reg.finish(&first, SubagentStatus::Succeeded, Some("first done".into()));
        assert!(!reg.agent_wake_ready());
        assert!(reg.pending_completed_for_agent().is_empty());

        reg.finish(
            &second,
            SubagentStatus::Succeeded,
            Some("second done".into()),
        );
        assert!(reg.agent_wake_ready());
        let completed = reg.drain_completed_for_agent();
        assert_eq!(completed.len(), 2);
        assert!(
            completed
                .iter()
                .all(|item| item.snapshot.launch_group_id.as_deref() == Some("group-9"))
        );
        assert!(!reg.agent_wake_ready());
    }

    #[test]
    fn attached_completion_does_not_wake_agent() {
        let reg = SubagentRegistry::new();
        let id = reg.register("explore", "p", false);
        reg.attach_tool_call(&id, "call-1").unwrap();
        reg.finish(&id, SubagentStatus::Succeeded, Some("done".into()));

        assert!(!reg.agent_wake_ready());
        assert!(reg.drain_completed_for_agent().is_empty());
    }

    #[test]
    fn recording_sink_captures_tools_and_text() {
        let reg = Arc::new(SubagentRegistry::new());
        let id = reg.register("explore", "p", false);
        let sink = RecordingSink::new(reg.clone(), id.clone());
        sink.tool_started("t1", "grep", "{\"query\":\"Skill\"}");
        sink.tool_finished(
            "t1",
            "3 matches",
            crate::output::ToolExecutionStatus::Succeeded,
        );
        sink.assistant_delta("Found it at ");
        sink.assistant_delta("src/lib.rs:1");
        sink.assistant_done();
        let snap = reg.snapshot(&id).unwrap();
        assert!(snap.activity.contains("▸ grep"));
        assert!(snap.activity.contains("ok"));
        assert!(snap.activity.contains("3 matches"));
        assert!(snap.activity.contains("Found it at src/lib.rs:1"));
        let partial = snap.partial_result().unwrap();
        assert!(partial.contains("<<<untrusted-content source=\"subagent:partial\">>>"));
        assert!(partial.contains("src/lib.rs:1"));
    }

    #[test]
    fn running_model_status_is_stable_and_excludes_prompt_activity() {
        let reg = SubagentRegistry::new();
        let id = reg.register("explore", "SECRET-PROMPT: inspect src/private.rs", true);
        reg.set_model(&id, "test-model".to_string());

        let before = reg
            .running_status_report()
            .expect("running subagent should produce status");
        reg.append_activity(&id, "SECRET-ACTIVITY: opened private source");
        let after = reg
            .running_status_report()
            .expect("running subagent should still produce status");

        assert_eq!(before, after, "activity must not churn model context");
        assert!(after.contains(&id));
        assert!(after.contains("explore"));
        assert!(after.contains("test-model"));
        assert!(!after.contains("SECRET-PROMPT"), "status: {after}");
        assert!(!after.contains("SECRET-ACTIVITY"), "status: {after}");
        assert!(!after.contains("Activity"), "status: {after}");
    }

    #[test]
    fn completion_model_context_contains_results_not_ui_detail() {
        let reg = SubagentRegistry::new();
        let first = reg.register("explore", "SECRET-PROMPT-ONE", true);
        let second = reg.register("review", "SECRET-PROMPT-TWO", true);
        reg.attach_tool_call_in_group(&first, "call-1", Some("group-1".to_string()))
            .unwrap();
        reg.attach_tool_call_in_group(&second, "call-2", Some("group-1".to_string()))
            .unwrap();
        reg.append_activity(&first, "SECRET-ACTIVITY-ONE");
        reg.append_activity(&second, "SECRET-ACTIVITY-TWO");
        reg.finish(
            &first,
            SubagentStatus::Succeeded,
            Some("finding at src/a.rs:10".to_string()),
        );
        reg.finish(
            &second,
            SubagentStatus::Succeeded,
            Some("no substantive findings".to_string()),
        );

        let message = subagent_completion_message(&reg.drain_completed_for_agent());

        assert!(message.contains("finding at src/a.rs:10"));
        assert!(message.contains("no substantive findings"));
        assert!(!message.contains("SECRET-PROMPT"), "message: {message}");
        assert!(!message.contains("SECRET-ACTIVITY"), "message: {message}");
        assert!(!message.contains("Prompt:"), "message: {message}");
        assert!(!message.contains("Activity:"), "message: {message}");
    }

    #[test]
    fn activity_is_front_trimmed_at_cap() {
        let reg = Arc::new(SubagentRegistry::new());
        let id = reg.register("explore", "p", false);
        let sink = RecordingSink::new(reg.clone(), id.clone());
        let big = "x".repeat(ACTIVITY_CAP_CHARS + 500);
        sink.assistant_delta(&big);
        let snap = reg.snapshot(&id).unwrap();
        assert!(snap.activity.chars().count() <= ACTIVITY_CAP_CHARS);
    }

    #[test]
    fn list_returns_sorted_by_id() {
        let reg = SubagentRegistry::new();
        reg.register("explore", "a", false);
        reg.register("review", "b", false);
        let ids: Vec<_> = reg.list().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![Arc::from("sub-1"), Arc::from("sub-2")]);
    }

    #[test]
    fn list_orders_numerically_past_nine() {
        // A `BTreeMap<String, _>` would order these `sub-1, sub-10, sub-11, sub-2,
        // …`; the numeric key keeps them chronological.
        let reg = SubagentRegistry::new();
        for _ in 0..11 {
            reg.register("explore", "p", false);
        }
        let ids: Vec<_> = reg.list().into_iter().map(|s| s.id).collect();
        assert_eq!(ids.first().map(Arc::as_ref), Some("sub-1"));
        assert_eq!(ids.get(1).map(Arc::as_ref), Some("sub-2"));
        assert_eq!(ids.get(9).map(Arc::as_ref), Some("sub-10"));
        assert_eq!(ids.last().map(Arc::as_ref), Some("sub-11"));
    }

    #[test]
    fn finish_preserves_result_newlines() {
        let reg = SubagentRegistry::new();
        let id = reg.register("review", "p", false);
        reg.finish(
            &id,
            SubagentStatus::Succeeded,
            Some("line one\nline two".into()),
        );
        assert_eq!(
            reg.snapshot(&id).unwrap().result.as_deref(),
            Some("line one\nline two")
        );
    }

    #[tokio::test]
    async fn wait_for_subagent_returns_finished_snapshot() {
        let reg = Arc::new(SubagentRegistry::new());
        let id = reg.register("research", "p", true);
        let finisher = reg.clone();
        let finish_id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            finisher.finish(&finish_id, SubagentStatus::Succeeded, Some("done".into()));
        });

        let snap = reg
            .wait_for_subagent(&id, Duration::from_secs(1))
            .await
            .expect("subagent should exist");

        assert_eq!(snap.status, SubagentStatus::Succeeded);
        assert_eq!(snap.result.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn wait_for_unknown_subagent_returns_none() {
        let reg = SubagentRegistry::new();

        assert!(
            reg.wait_for_subagent("sub-404", Duration::from_millis(1))
                .await
                .is_none()
        );
    }
}
