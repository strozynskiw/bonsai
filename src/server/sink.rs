use std::sync::{Arc, Mutex};

use crate::completion_report::{CompletionEvidenceCollector, CompletionEvidenceSnapshot};
use crate::interaction::InteractionRequest;
use crate::output::{OutputSink, ToolCallStart, ToolExecutionStatus};
use crate::protocol::{ToolResult, TranscriptItem};
use crate::runtime::transcript::TranscriptRecorder;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub(crate) enum SessionEvent {
    ResyncRequired,
    UserMessage(String),
    AssistantDelta(String),
    AssistantDone,
    ReasoningDelta(String),
    AttemptStarted,
    AttemptDiscarded,
    ToolStarted(ToolCallStart),
    ToolOutput {
        id: String,
        text: String,
    },
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        cost_micros: Option<u64>,
    },
    Plan(Box<crate::plan::PlanDoc>),
    Todos(Vec<crate::todo::TodoItem>),
    Completion(Box<crate::completion_report::CompletionReport>),
    ToolResult {
        id: String,
        result: ToolResult,
    },
    Status(String),
    Error(String),
    Interaction(InteractionRequest),
    InteractionResolved(u64),
    Lifecycle {
        run_id: Option<String>,
        state: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct OrderedEvent {
    pub(crate) sequence: u64,
    pub(crate) event: SessionEvent,
}

#[derive(Debug, Default)]
struct State {
    sequence: u64,
    prefix: Vec<TranscriptItem>,
    usage: (u64, u64, Option<u64>),
    plan_revision: Option<u64>,
    todos: Option<Vec<crate::todo::TodoItem>>,
    replay: std::collections::VecDeque<(OrderedEvent, usize)>,
    replay_bytes: usize,
}

impl State {
    fn emit(&mut self, events: &broadcast::Sender<OrderedEvent>, event: SessionEvent) {
        self.sequence = self.sequence.saturating_add(1);
        let event = OrderedEvent {
            sequence: self.sequence,
            event,
        };
        let bytes = super::contract::event(&event.event).to_string().len();
        if bytes <= 8 * 1024 * 1024 {
            self.replay_bytes += bytes;
            self.replay.push_back((event.clone(), bytes));
        } else {
            self.replay.clear();
            self.replay_bytes = 0;
        }
        while self.replay.len() > 1000 || self.replay_bytes > 8 * 1024 * 1024 {
            if let Some((_, bytes)) = self.replay.pop_front() {
                self.replay_bytes -= bytes;
            }
        }
        // 256 live slots * 32 KiB bounds retained live payloads to 8 MiB.
        let live = if bytes > 32 * 1024 {
            OrderedEvent {
                sequence: event.sequence,
                event: SessionEvent::ResyncRequired,
            }
        } else {
            event
        };
        let _ = events.send(live);
    }
}

/// Synchronous recording and publication share one ordering fence. Receivers that
/// lag must resnapshot; this channel is not a durable event log.
#[derive(Debug)]
pub(crate) struct SessionSink {
    generation: uuid::Uuid,
    state: Mutex<State>,
    recorder: TranscriptRecorder,
    evidence: CompletionEvidenceCollector,
    events: broadcast::Sender<OrderedEvent>,
}

impl SessionSink {
    pub(crate) fn new(prefix: Vec<TranscriptItem>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            generation: uuid::Uuid::now_v7(),
            state: Mutex::new(State {
                prefix,
                ..State::default()
            }),
            recorder: TranscriptRecorder::default(),
            evidence: CompletionEvidenceCollector::default(),
            events,
        })
    }

    pub(crate) fn generation(&self) -> uuid::Uuid {
        self.generation
    }

    fn record(&self, event: SessionEvent, record: impl FnOnce(&TranscriptRecorder)) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        record(&self.recorder);
        state.emit(&self.events, event);
    }

    /// Project existing stores; coalesced notifications carry full current values.
    pub(crate) fn domain_updated(
        &self,
        plan: &crate::plan::PlanDoc,
        todos: &[crate::todo::TodoItem],
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.plan_revision != Some(plan.revision) {
            state.plan_revision = Some(plan.revision);
            state.emit(&self.events, SessionEvent::Plan(Box::new(plan.clone())));
        }
        if state.todos.as_deref() != Some(todos) {
            state.todos = Some(todos.to_vec());
            state.emit(&self.events, SessionEvent::Todos(todos.to_vec()));
        }
    }

    pub(crate) fn publish(&self, event: SessionEvent) {
        self.record(event, |_| {});
    }

    pub(crate) fn user_message(&self, text: &str) {
        self.record(SessionEvent::UserMessage(text.into()), |r| {
            r.user_message(text)
        });
    }

    /// Snapshot and subscription are acquired under the publication fence.
    pub(crate) fn subscribe(
        &self,
    ) -> (u64, Vec<TranscriptItem>, broadcast::Receiver<OrderedEvent>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut transcript = state.prefix.clone();
        transcript.extend(self.recorder.snapshot());
        (state.sequence, transcript, self.events.subscribe())
    }

    /// Replay only through the subscription watermark; reject any missing interval.
    pub(crate) fn replay(&self, after: u64, through: u64) -> Option<Vec<OrderedEvent>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if after > through {
            return None;
        }
        let events: Vec<_> = state
            .replay
            .iter()
            .filter(|(e, _)| e.sequence > after && e.sequence <= through)
            .map(|(e, _)| e.clone())
            .collect();
        if events.len() as u64 != through - after {
            return None;
        }
        Some(events)
    }

    pub(crate) fn usage(&self) -> (u64, u64, Option<u64>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .usage
    }

    pub(crate) fn transcript(&self) -> Vec<TranscriptItem> {
        self.subscribe().1
    }

    pub(crate) fn completion_evidence(&self) -> CompletionEvidenceSnapshot {
        self.evidence.snapshot()
    }

    pub(crate) fn completion(&self, report: &crate::completion_report::CompletionReport) {
        self.record(SessionEvent::Completion(Box::new(report.clone())), |r| {
            r.push(crate::protocol::TranscriptItem::CompletionReport(Box::new(
                report.clone(),
            )))
        });
    }
}

impl OutputSink for SessionSink {
    fn completion_evidence(&self) -> Option<CompletionEvidenceSnapshot> {
        Some(self.evidence.snapshot())
    }
    fn begin_completion_run(&self) {
        self.evidence.begin_run();
    }
    fn assistant_delta(&self, text: &str) {
        self.record(SessionEvent::AssistantDelta(text.into()), |r| {
            r.assistant_delta(text)
        });
    }
    fn assistant_done(&self) {
        self.record(
            SessionEvent::AssistantDone,
            TranscriptRecorder::assistant_done,
        );
    }
    fn thinking(&self, text: &str) {
        self.reasoning_delta(text);
    }
    fn context_updated(&self, report: crate::agent::ContextReport) {
        let usage = (
            report.session_prompt_tokens,
            report.session_completion_tokens,
            report.session_cost_micros,
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.usage = usage;
        state.emit(
            &self.events,
            SessionEvent::Usage {
                prompt_tokens: usage.0,
                completion_tokens: usage.1,
                cost_micros: usage.2,
            },
        );
    }
    fn tool_output(&self, id: &str, output: &str) {
        self.record(
            SessionEvent::ToolOutput {
                id: id.into(),
                text: output.into(),
            },
            |r| {
                r.tool_result(
                    id,
                    ToolResult::legacy(output.into(), ToolExecutionStatus::Started),
                )
            },
        );
    }
    fn reasoning_delta(&self, text: &str) {
        self.record(SessionEvent::ReasoningDelta(text.into()), |r| {
            r.reasoning_delta(text)
        });
    }
    fn attempt_started(&self) {
        self.record(
            SessionEvent::AttemptStarted,
            TranscriptRecorder::attempt_started,
        );
    }
    fn attempt_discarded(&self) {
        self.record(
            SessionEvent::AttemptDiscarded,
            TranscriptRecorder::attempt_discarded,
        );
    }
    fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        self.evidence.record_tool_started(id, name, arguments);
        self.record(
            SessionEvent::ToolStarted(ToolCallStart {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            }),
            |r| r.tool_started(id, name, arguments),
        );
    }
    fn tool_calls_started(&self, calls: &[ToolCallStart]) {
        for call in calls {
            self.tool_started(&call.id, &call.name, &call.arguments);
        }
    }
    fn tool_result(&self, id: &str, result: ToolResult) {
        self.evidence
            .record_tool_result(id, &result.preview, result.status, result.diff());
        self.record(
            SessionEvent::ToolResult {
                id: id.into(),
                result: result.clone(),
            },
            |r| r.tool_result(id, result),
        );
    }
    fn tool_finished(&self, id: &str, result: &str, status: ToolExecutionStatus) {
        self.tool_result(id, ToolResult::legacy(result.into(), status));
    }
    fn tool_finished_with_diff(
        &self,
        id: &str,
        summary: &str,
        status: ToolExecutionStatus,
        diff: crate::diff::FileDiff,
    ) {
        let mut result = ToolResult::legacy(summary.into(), status);
        result.content = crate::protocol::ToolResultContent::Edit {
            summary: summary.into(),
            diff: Box::new(diff),
        };
        self.tool_result(id, result);
    }
    fn workspace_changed(&self, paths: &[String], intent: &str) {
        self.evidence.record_workspace_changes(paths, intent);
    }
    fn status(&self, message: &str) {
        self.record(SessionEvent::Status(message.into()), |r| r.status(message));
    }
    fn error(&self, message: &str) {
        self.record(SessionEvent::Error(message.into()), |r| {
            r.push(crate::protocol::TranscriptItem::CommandOutput {
                kind: crate::protocol::CommandOutputKind::Error,
                text: message.into(),
            })
        });
    }
}
