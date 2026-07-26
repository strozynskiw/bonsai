//! Deterministic episode-boundary detection. Successful title-bearing tools
//! declare whether they represent a new topic or a refinement of the current
//! one. The observer records that signal, and resolution happens at the next
//! preflight, when the complete assistant + tool-result group exists in
//! context. No extra LLM classifier calls, ever.

use super::*;
use crate::episode::{EpisodeAction, PendingEpisodeTitleSignal};

/// A human-submitted user turn: `User` role with no provenance name. Harness,
/// peer, background, and untrusted notes are user-role too but always carry a
/// wire name, so they never anchor a boundary.
pub(super) fn is_human_user_message(message: &ChatCompletionRequestMessage) -> bool {
    matches!(message, ChatCompletionRequestMessage::User(user) if user.name.is_none())
}

#[derive(serde::Deserialize)]
struct EpisodeTitleSignalArgs {
    title: String,
    #[serde(default)]
    episode_action: EpisodeAction,
}

/// Parse title-bearing tool arguments with the title tool's exact validation,
/// so the ledger title always matches the persisted session summary byte-for-byte.
fn episode_title_signal_from_arguments(arguments: &str) -> Option<(String, EpisodeAction)> {
    let args: EpisodeTitleSignalArgs = serde_json::from_str(arguments).ok()?;
    let title = crate::tool::normalize_session_title(&args.title)
        .ok()?
        .to_string();
    Some((title, args.episode_action))
}

/// Paths a span's tool calls touched, deduped in first-seen order. Uses the
/// same `path` argument probe compaction's summary uses.
fn files_touched_in_span(messages: &[ChatCompletionRequestMessage]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for message in messages {
        let Some(calls) = assistant_tool_calls(message) else {
            continue;
        };
        for call in calls {
            let Some(path) = tool_path_argument(&call.arguments) else {
                continue;
            };
            if seen.insert(path.clone()) {
                files.push(path);
            }
        }
    }
    files
}

fn tool_path_argument(arguments: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get("path")?
        .as_str()
        .map(str::to_string)
}

impl Agent {
    /// `apply_tool_result` hook: record a successful title-bearing tool as an
    /// episode signal. Runs before the result message is appended, so only the
    /// signal and call id are captured — resolution waits for preflight.
    pub(in crate::agent) fn observe_episode_title_result(&mut self, tool_call: &ToolCall) {
        if self.episode_store.is_none()
            || !matches!(
                tool_call.name.as_str(),
                "set_session_title" | "plan_replace_draft"
            )
        {
            return;
        }
        let Some((title, action)) = episode_title_signal_from_arguments(&tool_call.arguments)
        else {
            return;
        };
        let Some(mut ledger) = self.episode_ledger() else {
            return;
        };
        ledger.set_pending_title_signal(PendingEpisodeTitleSignal {
            title,
            call_id: tool_call.id.clone(),
            action,
        });
    }

    /// Arm the active episode when a successful `todowrite` leaves no pending
    /// work. Closing waits until preflight, after the result has been appended
    /// and the whole assistant/tool group is known to be complete.
    pub(in crate::agent) async fn observe_todowrite_result(&mut self) {
        if self.episode_store.is_none() {
            return;
        }
        let all_done = if let Some(store) = &self.todo_store {
            let store = store.lock().await;
            !store.todos().is_empty()
                && store.todos().iter().all(|todo| {
                    matches!(
                        todo.status,
                        crate::todo::TodoStatus::Completed | crate::todo::TodoStatus::Cancelled
                    )
                })
        } else {
            false
        };
        if !all_done {
            return;
        }
        if let Some(mut ledger) = self.episode_ledger() {
            ledger.mark_active_completable();
        }
    }

    /// Close an episode whose todo list reached a terminal state. The final
    /// todowrite group remains part of the closed span, so its card and archive
    /// contain the evidence that work completed. No successor opens until a
    /// later human turn starts a new task.
    pub(in crate::agent) fn apply_episode_completion_boundary(&mut self) {
        let active = self
            .episode_ledger()
            .and_then(|ledger| ledger.active().cloned());
        let Some(active) = active.filter(|episode| episode.is_completable()) else {
            return;
        };
        let Some(start_index) = self
            .message_ids
            .iter()
            .position(|id| id == active.start_stable_id())
        else {
            return;
        };
        let groups = compaction::message_groups(&self.messages);
        let Some(last_group) = groups.last() else {
            return;
        };
        if !message_group_is_complete(&self.messages, last_group)
            || last_group.indices.last().copied() != self.messages.len().checked_sub(1)
        {
            return;
        }
        let Some(end_index) = last_group.indices.last().copied() else {
            return;
        };
        if end_index < start_index {
            return;
        }
        let end_stable_id = self.message_ids[end_index].clone();
        let files_touched = files_touched_in_span(&self.messages[start_index..=end_index]);
        let now = crate::util::time::now_ms();
        if let Some(mut ledger) = self.episode_ledger()
            && let Some(seq) = ledger.close_active(
                crate::episode::EpisodeCloseReason::TodoComplete,
                end_stable_id,
                now,
            )
        {
            ledger.set_closed_files_touched(seq, files_touched);
        }
    }

    /// Roll a long active episode before it can consume the full session. The
    /// oldest complete groups close once they exceed 25% of the usable input
    /// window; the latest K groups stay live in a same-title successor. A live
    /// read-reuse pointer into the candidate span defers the boundary because
    /// evicting its target would make the pointer invalid.
    pub(in crate::agent) fn apply_episode_size_boundary(
        &mut self,
        tool_schema: &ToolSchemaPayload,
    ) {
        let active = self
            .episode_ledger()
            .and_then(|ledger| ledger.active().cloned());
        let Some(active) = active else {
            return;
        };
        let Some(start_index) = self
            .message_ids
            .iter()
            .position(|id| id == active.start_stable_id())
        else {
            return;
        };
        let groups = compaction::message_groups(&self.messages)
            .into_iter()
            .filter(|group| {
                group
                    .indices
                    .first()
                    .is_some_and(|index| *index >= start_index)
                    && message_group_is_complete(&self.messages, group)
            })
            .collect::<Vec<_>>();
        let minimum_groups =
            EPISODE_MIN_CLOSED_GROUPS.saturating_add(EPISODE_SIZE_PROTECTED_TAIL_GROUPS);
        if groups.len() < minimum_groups {
            return;
        }
        let boundary_group = groups.len() - EPISODE_SIZE_PROTECTED_TAIL_GROUPS;
        let Some(boundary) = groups[boundary_group].indices.first().copied() else {
            return;
        };
        if boundary <= start_index {
            return;
        }
        let pointer_targets = self.read_reuse_target_indices();
        if pointer_targets.iter().any(|index| {
            *index >= start_index
                && *index < boundary
                && self.read_reuse_target_referenced_outside_span(*index, start_index, boundary - 1)
        }) {
            return;
        }
        let message_ids = self.message_ids_for_messages(self.messages.len());
        let (message_tokens, _estimate) = self.payload_message_token_estimate(
            &self.messages,
            &message_ids,
            &self.context_controls,
            tool_schema,
        );
        let span_tokens = message_tokens[start_index..boundary]
            .iter()
            .copied()
            .sum::<usize>();
        let usable_tokens = self
            .budget
            .context_budget_tokens
            .saturating_sub(self.output_reserve_tokens())
            .max(1);
        let trigger_tokens = usable_tokens
            .saturating_mul(EPISODE_SIZE_TRIGGER_PERCENT)
            .checked_div(100)
            .unwrap_or(usable_tokens)
            .max(1);
        if span_tokens < trigger_tokens {
            return;
        }

        let end_index = boundary - 1;
        let end_stable_id = self.message_ids[end_index].clone();
        let successor_start_id = self.message_ids[boundary].clone();
        let files_touched = files_touched_in_span(&self.messages[start_index..boundary]);
        let title = active.title().to_string();
        let goal = active.goal().to_string();
        let now = crate::util::time::now_ms();
        if let Some(mut ledger) = self.episode_ledger()
            && let Some(seq) = ledger.close_active(
                crate::episode::EpisodeCloseReason::SizePressure,
                end_stable_id,
                now,
            )
        {
            ledger.set_closed_files_touched(seq, files_touched);
            ledger.open_episode(successor_start_id, title, goal, now);
        }
    }

    /// Consume a pending title signal at the top of preflight — the only point
    /// where the turn is guaranteed to sit at a complete group boundary.
    /// Closing is pure bookkeeping (zero wire-byte change); eviction is a
    /// separate, later step.
    ///
    /// Boundary rules (deterministic):
    /// - `g` — first message index of the group containing the signaled call;
    /// - `u` — index of the last human user message with `start < u < g`;
    /// - `same_topic` always renames the active episode;
    /// - `new_topic` closes at `u` only when `u` exists and the size guard is
    ///   met; without a human anchor it also renames in place.
    ///
    /// The active episode closes over `[start ..= b-1]`; the successor opens at
    /// `b` under the new title. First titling of an untitled episode and
    /// same-title repeats never close; an episode below the minimum-size guard
    /// is retitled in place instead of closed.
    pub(in crate::agent) fn apply_pending_episode_title_signal(&mut self) {
        if self.episode_store.is_none() {
            return;
        }
        let Some(signal) = self
            .episode_ledger()
            .and_then(|mut ledger| ledger.take_pending_title_signal())
        else {
            return;
        };

        // Locate the assistant message carrying the signaled call. Absent
        // means the turn was discarded (interrupt/steering rewind) or already
        // compacted away — the signal is void, not deferred forever.
        let Some(group_start) = self.messages.iter().position(|message| {
            assistant_tool_calls(message)
                .is_some_and(|calls| calls.iter().any(|call| call.call_id == signal.call_id))
        }) else {
            return;
        };

        // An incomplete group (an interrupted multi-call batch still missing
        // tool results) defers the close rather than splitting wire state.
        let call_count = assistant_tool_calls(&self.messages[group_start])
            .map(|calls| calls.len())
            .unwrap_or(0);
        let groups = compaction::message_groups(&self.messages);
        let group_complete = groups
            .iter()
            .find(|group| group.indices.first() == Some(&group_start))
            .is_some_and(|group| group.indices.len() == call_count + 1);
        if !group_complete {
            if let Some(mut ledger) = self.episode_ledger() {
                ledger.set_pending_title_signal(signal);
            }
            return;
        }

        let now = crate::util::time::now_ms();
        let active = self
            .episode_ledger()
            .and_then(|ledger| ledger.active().cloned());

        let Some(active) = active else {
            // No active episode (e.g. a `/start`-seeded run titles itself):
            // open one at the boundary instead of leaving the span untracked.
            let boundary = self
                .last_human_user_index_before(group_start, 0)
                .unwrap_or(group_start);
            self.open_episode_at_index(boundary, signal.title, now);
            return;
        };

        if titles_equivalent(active.title(), &signal.title) {
            return;
        }

        if signal.action == EpisodeAction::SameTopic {
            if let Some(mut ledger) = self.episode_ledger() {
                ledger.rename_active(signal.title);
            }
            return;
        }

        // The span start; when compaction already ate the start row, fall back
        // to the first non-system index so guard counting stays defined.
        let start_index = self
            .message_ids
            .iter()
            .position(|id| id == active.start_stable_id())
            .unwrap_or(1);

        if active.title().is_empty() {
            // First titling never closes. If an untitled greeting preceded a
            // later explicit new-topic request, move the episode anchor to that
            // request so its goal and eventual archive describe the real task.
            let reanchor = self
                .last_human_user_index_before(group_start, start_index)
                .and_then(|boundary| {
                    let start_id = self.message_ids.get(boundary)?.clone();
                    let (_role, text) = describe_message_full(&self.messages[boundary]);
                    Some((start_id, episode_goal_line(&text)))
                });
            if let Some(mut ledger) = self.episode_ledger() {
                if let Some((start_id, goal)) = reanchor {
                    ledger.reanchor_active(start_id, signal.title, goal);
                } else {
                    ledger.rename_active(signal.title);
                }
            }
            return;
        }

        // A model-only phase/title change has no human new-topic anchor. Keep
        // one episode and rename it instead of manufacturing an empty-goal
        // successor at the title tool group.
        let Some(boundary) = self.last_human_user_index_before(group_start, start_index) else {
            if let Some(mut ledger) = self.episode_ledger() {
                ledger.rename_active(signal.title);
            }
            return;
        };

        // Size guard: the closing span must hold at least
        // EPISODE_MIN_CLOSED_GROUPS completed groups, one of them a tool
        // group. Too small → the title change renames the episode in place.
        let span_groups = groups
            .iter()
            .filter(|group| {
                group
                    .indices
                    .first()
                    .is_some_and(|first| *first >= start_index)
                    && group.indices.last().is_some_and(|last| *last < boundary)
            })
            .collect::<Vec<_>>();
        let complete_span_groups = span_groups
            .iter()
            .filter(|group| {
                let first = group.indices[0];
                let calls = assistant_tool_calls(&self.messages[first])
                    .map(|calls| calls.len())
                    .unwrap_or(0);
                calls == 0 || group.indices.len() == calls + 1
            })
            .count();
        // "Tool group" means real work: a completed group whose assistant turn
        // called something other than set_session_title. Without this, pure
        // title churn (user msg + retitle group) would satisfy the size guard
        // and shred the timeline into empty episodes.
        let has_work_group = span_groups.iter().any(|group| {
            let has_tool_result = group.indices.iter().any(|index| {
                matches!(self.messages[*index], ChatCompletionRequestMessage::Tool(_))
            });
            has_tool_result
                && assistant_tool_calls(&self.messages[group.indices[0]])
                    .is_some_and(|calls| calls.iter().any(|call| call.name != "set_session_title"))
        });
        if boundary <= start_index
            || complete_span_groups < EPISODE_MIN_CLOSED_GROUPS
            || !has_work_group
        {
            if let Some(mut ledger) = self.episode_ledger() {
                ledger.rename_active(signal.title);
            }
            return;
        }

        // Close [start ..= boundary-1] and open the successor at the boundary.
        let end_stable_id = self.message_ids[boundary - 1].clone();
        let files_touched = files_touched_in_span(&self.messages[start_index..boundary]);
        if let Some(mut ledger) = self.episode_ledger()
            && let Some(seq) = ledger.close_active(
                crate::episode::EpisodeCloseReason::TitleChange,
                end_stable_id,
                now,
            )
        {
            ledger.set_closed_files_touched(seq, files_touched);
        }
        self.open_episode_at_index(boundary, signal.title, now);
    }

    /// The last human user message index strictly inside `(floor, ceiling)`.
    fn last_human_user_index_before(&self, ceiling: usize, floor: usize) -> Option<usize> {
        self.messages[..ceiling]
            .iter()
            .rposition(is_human_user_message)
            .filter(|index| *index > floor)
    }

    /// Open a successor episode whose span starts at `index`. The goal comes
    /// from that row only when it is a human user message — a successor opened
    /// on a retitle group has no opening request to quote.
    fn open_episode_at_index(&mut self, index: usize, title: String, now: i64) {
        let Some(start_id) = self.message_ids.get(index).cloned() else {
            return;
        };
        let goal = if is_human_user_message(&self.messages[index]) {
            let (_role, text) = describe_message_full(&self.messages[index]);
            episode_goal_line(&text)
        } else {
            String::new()
        };
        if let Some(mut ledger) = self.episode_ledger() {
            ledger.open_episode(start_id, title, goal, now);
        }
    }
}

/// Case-folded title comparison: a cosmetic re-emit of the same title must not
/// arm a close.
fn titles_equivalent(current: &str, next: &str) -> bool {
    current.trim().to_lowercase() == next.trim().to_lowercase()
}

fn message_group_is_complete(
    messages: &[ChatCompletionRequestMessage],
    group: &compaction::types::MessageGroup,
) -> bool {
    let Some(first) = group.indices.first().copied() else {
        return false;
    };
    let calls = assistant_tool_calls(&messages[first])
        .map(|calls| calls.len())
        .unwrap_or(0);
    calls == 0 || group.indices.len() == calls.saturating_add(1)
}
