use std::time::Instant;

use super::*;
use crate::tui::event::{CommandOutputKind, Focus};

impl AppState {
    /// Locates the item index first and only then borrows it mutably via
    /// `get_mut`, so the transcript layout cache invalidates exactly the one
    /// item hosting this tool (an `iter_mut` search would pessimistically
    /// re-render the whole transcript on every tool status/output update).
    fn tool_activity_mut(&mut self, tool_id: &str) -> Option<&mut ToolActivity> {
        let index = self.transcript.iter().position(|item| match item {
            TranscriptItem::ToolActivity(activity) => activity.id == tool_id,
            TranscriptItem::ExecutionGroup(group) => {
                group.tools.iter().any(|activity| activity.id == tool_id)
            }
            _ => false,
        })?;
        match self.transcript.get_mut(index)? {
            TranscriptItem::ToolActivity(activity) => Some(activity),
            TranscriptItem::ExecutionGroup(group) => group
                .tools
                .iter_mut()
                .find(|activity| activity.id == tool_id),
            _ => None,
        }
    }

    /// Walks with `iter_mut` (via `&mut self.transcript`), which bumps every
    /// item's layout-cache revision. Deliberate: this fires once per
    /// permission re-prompt, not per frame, and the matching tool's host item
    /// is not known up front.
    pub(super) fn reset_permission_tool_timer(&mut self, command: &str, started_at: Instant) {
        let mut reset = false;
        for item in &mut self.transcript {
            match item {
                TranscriptItem::ToolActivity(activity)
                    if tool_waits_for_permission_command(activity, command) =>
                {
                    activity.started_at = started_at;
                    activity.finished_at = None;
                    reset = true;
                }
                TranscriptItem::ExecutionGroup(group) => {
                    if let Some(activity) = group
                        .tools
                        .iter_mut()
                        .find(|activity| tool_waits_for_permission_command(activity, command))
                    {
                        activity.started_at = started_at;
                        activity.finished_at = None;
                        reset = true;
                    }
                }
                _ => {}
            }
            if reset {
                break;
            }
        }
        if reset {
            self.recompute_active_tools();
        }
    }

    pub(super) fn recompute_active_tools(&mut self) {
        self.active_tools.clear();
        for item in &self.transcript {
            match item {
                TranscriptItem::ToolActivity(activity)
                    if matches!(activity.status, ToolStatus::Running) =>
                {
                    self.active_tools
                        .push((activity.name.clone(), activity.started_at));
                }
                TranscriptItem::ExecutionGroup(group) => {
                    for activity in &group.tools {
                        if matches!(activity.status, ToolStatus::Running) {
                            self.active_tools
                                .push((activity.name.clone(), activity.started_at));
                        }
                    }
                }
                _ => {}
            }
        }
        // Stable sort by started_at — `Vec::sort_by_key` is stable.
        self.active_tools.sort_by_key(|(_, started_at)| *started_at);
    }

    /// Turn-end safety net: flip any tool still marked `Running` to a terminal
    /// state and stamp `finished_at`. A tool's `ToolFinished` event can go
    /// missing — most visibly the self-review reviewer, whose card is closed by
    /// a single direct `tool_finished` and, unlike detached subagents (which
    /// self-heal via `apply_completed_subagent_tool_calls`), has no
    /// reconciliation lane. When that event is lost the card's spinner animates
    /// forever and `group_duration` counts to `Instant::now()` every frame (the
    /// ever-growing "331.2s"). Once the run is truly over, nothing legitimate is
    /// still Running except background bash — which is deliberately kept alive
    /// past the turn, so it is excluded here. A lingering tool that already
    /// recorded a result is treated as succeeded (it produced output); one with
    /// no result was cut off, so it is marked failed. Walks with `iter_mut`,
    /// bumping every item's layout-cache revision — acceptable because this
    /// fires once per turn end, not per frame.
    pub(super) fn reconcile_orphaned_running_tools(&mut self, finished_at: Instant) {
        for item in &mut self.transcript {
            let tools = match item {
                TranscriptItem::ToolActivity(activity) => std::slice::from_mut(activity),
                TranscriptItem::ExecutionGroup(group) => group.tools.as_mut_slice(),
                _ => continue,
            };
            for activity in tools {
                if !matches!(activity.status, ToolStatus::Running)
                    || is_background_bash_call(activity)
                {
                    continue;
                }
                activity.status = if activity.result.is_some() {
                    ToolStatus::Succeeded
                } else {
                    ToolStatus::Failed
                };
                activity.finished_at = Some(finished_at);
            }
        }
    }

    /// Adopt each subagent run's model, keyed by the `agent` tool call that
    /// launched it, so the call's detail view shows what the run actually
    /// uses (not the parent turn's model). Returns whether anything changed,
    /// so the caller can skip a redraw on the common no-op refresh.
    pub(crate) fn adopt_subagent_models(&mut self, assignments: &[(String, String)]) -> bool {
        let mut changed = false;
        for (tool_call_id, model) in assignments {
            if self.subagent_models.get(tool_call_id) != Some(model) {
                self.subagent_models
                    .insert(tool_call_id.clone(), model.clone());
                changed = true;
            }
        }
        changed
    }

    /// The model the subagent run launched by this `agent` tool call uses,
    /// once adopted from the registry. `None` for every other tool.
    pub fn subagent_model_for(&self, tool_id: &str) -> Option<&str> {
        self.subagent_models.get(tool_id).map(String::as_str)
    }

    pub fn tool_activity(&self, tool_id: &str) -> Option<&ToolActivity> {
        self.transcript.iter().find_map(|item| match item {
            TranscriptItem::ToolActivity(activity) if activity.id == tool_id => Some(activity),
            TranscriptItem::ExecutionGroup(group) => {
                group.tools.iter().find(|activity| activity.id == tool_id)
            }
            _ => None,
        })
    }

    pub fn latest_tool_id(&self) -> Option<String> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::ToolActivity(activity) => Some(activity.id.clone()),
            TranscriptItem::ExecutionGroup(group) => group
                .tools
                .iter()
                .rev()
                .find(|_activity| true)
                .map(|activity| activity.id.clone()),
            _ => None,
        })
    }

    pub(super) fn inline_selection_matches_tool(&self, tool_id: &str) -> bool {
        self.selected_execution_group_tool()
            .is_some_and(|activity| activity.id == tool_id)
    }

    pub(super) fn focus_tool_for_activation(&mut self, tool_id: &str) -> bool {
        let target =
            self.transcript
                .iter()
                .enumerate()
                .find_map(|(index, item)| match item {
                    TranscriptItem::ToolActivity(activity) if activity.id == tool_id => {
                        Some((index, None))
                    }
                    TranscriptItem::ExecutionGroup(group) => group
                        .tool_indices()
                        .enumerate()
                        .find_map(|(selected_tool, tool_index)| {
                            group
                                .tools
                                .get(tool_index)
                                .is_some_and(|activity| activity.id == tool_id)
                                .then_some((index, Some((group.id, selected_tool))))
                        }),
                    _ => None,
                });

        let Some((index, group_selection)) = target else {
            return false;
        };

        self.focus = Focus::Transcript;
        match group_selection {
            Some((group_id, selected_tool)) => {
                self.set_execution_group_tool_selection(group_id, selected_tool);
            }
            None => {
                self.active_group_tool_selection = None;
                self.set_transcript_focus(index);
            }
        }
        true
    }

    pub(super) fn finish_tool(
        &mut self,
        id: &str,
        result: String,
        status: crate::output::ToolExecutionStatus,
        finished_at: Instant,
    ) {
        if let Some(activity) = self.tool_activity_mut(id) {
            activity.status = ToolStatus::from_execution_status(status);
            activity.result = Some(merge_authorization_output(
                activity.result.as_deref(),
                &result,
            ));
            activity.finished_at = Some(finished_at);
            self.current_phase = Some(format!("Finished {}", activity.name));
            return;
        }

        self.push_transcript_item(TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text: result,
        });
    }

    pub(super) fn update_tool_output(&mut self, id: &str, output: String, _updated_at: Instant) {
        if let Some(activity) = self.tool_activity_mut(id) {
            activity.result = Some(merge_authorization_output(
                activity.result.as_deref(),
                &output,
            ));
            self.current_phase = Some(format!("Running {}", activity.name));
            return;
        }

        self.push_transcript_item(TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text: output,
        });
    }

    pub(super) fn finish_tool_with_diff(
        &mut self,
        id: &str,
        result: String,
        status: crate::output::ToolExecutionStatus,
        diff: crate::diff::FileDiff,
        finished_at: Instant,
    ) {
        if let Some(activity) = self.tool_activity_mut(id) {
            activity.status = ToolStatus::from_execution_status(status);
            activity.result = Some(merge_authorization_output(
                activity.result.as_deref(),
                &result,
            ));
            activity.diff = Some(diff);
            activity.finished_at = Some(finished_at);
            self.current_phase = Some(format!("Finished {}", activity.name));
            return;
        }

        self.push_transcript_item(TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text: result,
        });
    }
}

fn merge_authorization_output(existing: Option<&str>, incoming: &str) -> String {
    const MARKER: &str = "[authorization]";
    let existing_authorization = existing
        .into_iter()
        .flat_map(str::lines)
        .filter(|line| line.starts_with(MARKER))
        .collect::<Vec<_>>();

    if incoming.starts_with(MARKER) {
        let mut lines = existing_authorization;
        lines.extend(incoming.lines().filter(|line| line.starts_with(MARKER)));
        let authorization = lines.join("\n");
        let existing_body = existing
            .map(|value| {
                value
                    .lines()
                    .filter(|line| !line.starts_with(MARKER))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if existing_body.trim().is_empty() {
            authorization
        } else {
            format!("{authorization}\n\n{existing_body}")
        }
    } else if existing_authorization.is_empty() {
        incoming.to_string()
    } else {
        format!("{}\n\n{incoming}", existing_authorization.join("\n"))
    }
}

fn tool_waits_for_permission_command(activity: &ToolActivity, command: &str) -> bool {
    if !matches!(activity.status, ToolStatus::Running) {
        return false;
    }
    // The argument that carries the prompted-on value differs by tool: bash
    // prompts on `command`, WebFetch on `url`. WebSearch authorizes its configured
    // endpoint rather than a model argument, so any active domain prompt belongs
    // to its running call. On a cross-domain redirect the
    // prompt shows the redirect target, which won't match the activity's original
    // `url` — a harmless best-effort miss, like any other non-matching command.
    let arg_key = match activity.name.as_str() {
        "bash" => "command",
        "webfetch" => "url",
        "websearch" => return true,
        _ => return false,
    };
    let Ok(serde_json::Value::Object(map)) =
        serde_json::from_str::<serde_json::Value>(&activity.arguments)
    else {
        return false;
    };
    map.get(arg_key).and_then(serde_json::Value::as_str) == Some(command)
}

#[cfg(test)]
mod authorization_output_tests {
    use super::merge_authorization_output;

    #[test]
    fn authorization_lines_survive_streaming_and_final_output() {
        let authorization = "[authorization] allow · medium · workspace-write · fallback";
        let streaming = merge_authorization_output(Some(authorization), "building...");
        assert!(streaming.starts_with(authorization));
        assert!(streaming.ends_with("building..."));

        let final_output = merge_authorization_output(Some(&streaming), "done");
        assert!(final_output.starts_with(authorization));
        assert!(final_output.ends_with("done"));
        assert!(!final_output.contains("building..."));
    }
}
