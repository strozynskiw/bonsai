use std::time::Instant;

use super::*;
use crate::tui::event::Focus;

impl AppState {
    /// Drop finished tools from `active_tools` so the list tracks what is
    /// actually in flight. The list is kept sorted by `started_at` so
    /// the visible order matches emission order.
    pub(crate) fn execution_group(&self, group_id: u64) -> Option<&ExecutionGroup> {
        self.transcript.iter().find_map(|item| match item {
            TranscriptItem::ExecutionGroup(group) if group.id == group_id => Some(group),
            _ => None,
        })
    }

    pub(super) fn execution_group_index(&self, group_id: u64) -> Option<usize> {
        self.transcript.iter().position(
            |item| matches!(item, TranscriptItem::ExecutionGroup(group) if group.id == group_id),
        )
    }

    /// Index-precise (`position` + `get_mut`) so the layout cache invalidates
    /// only this group's item, not the whole transcript.
    pub(crate) fn execution_group_mut(&mut self, group_id: u64) -> Option<&mut ExecutionGroup> {
        let index = self.execution_group_index(group_id)?;
        match self.transcript.get_mut(index) {
            Some(TranscriptItem::ExecutionGroup(group)) => Some(group),
            _ => None,
        }
    }

    pub(crate) fn reset_next_execution_group_id(&mut self) {
        self.next_execution_group_id = self
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::ExecutionGroup(group) => Some(group.id),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
    }

    fn allocate_execution_group_id(&mut self) -> u64 {
        if self.next_execution_group_id == 1
            && self
                .transcript
                .iter()
                .any(|item| matches!(item, TranscriptItem::ExecutionGroup(_)))
        {
            self.reset_next_execution_group_id();
        }
        let group_id = self.next_execution_group_id;
        self.next_execution_group_id = self.next_execution_group_id.saturating_add(1);
        group_id
    }

    fn active_execution_group_can_accept_tools(&self, group_id: u64) -> bool {
        let Some(index) = self.transcript.first_trailing_queued_index().checked_sub(1) else {
            return false;
        };
        matches!(
            self.transcript.get(index),
            Some(TranscriptItem::ExecutionGroup(group)) if group.id == group_id
        )
    }

    pub(super) fn record_tool_started(
        &mut self,
        id: String,
        name: String,
        arguments: String,
        started_at: Instant,
    ) {
        self.record_tools_started(vec![ToolCallStart::new(id, name, arguments)], started_at);
    }

    pub(super) fn record_tools_started(&mut self, calls: Vec<ToolCallStart>, started_at: Instant) {
        let activities = calls
            .into_iter()
            .map(|call| {
                let arguments = display_tool_arguments(&call.arguments, &self.project_root);
                let mut activity = ToolActivity::new(call.id, call.name, arguments, started_at);
                self.adopt_pending_subagent_model(&mut activity);
                activity
            })
            .collect::<Vec<_>>();
        if activities.is_empty() {
            return;
        }

        if let Some(group_id) = self.active_execution_group_id
            && self.active_execution_group_can_accept_tools(group_id)
            && let Some(group) = self.execution_group_mut(group_id)
        {
            group.tools.extend(activities);
            self.recompute_active_tools();
            return;
        }

        self.close_active_execution_group();
        // A new tool group is opening. If the model's immediately-preceding
        // message was a telegraphic scratch note ("Need edit."), downgrade it
        // to a quiet work-log row now that we know a tool group follows. A
        // genuine final answer is never followed by a tool call, so this only
        // ever sees pre-tool narration — real answers stay assistant messages.
        if let Some(index) = self.transcript.first_trailing_queued_index().checked_sub(1)
            && let Some(TranscriptItem::AssistantMessage { text }) = self.transcript.get(index)
            && crate::tui::transcript::is_scratch_fragment(text)
        {
            self.transcript.reclassify_assistant_as_worklog(index);
        }
        let group_id = self.allocate_execution_group_id();
        let mut group = ExecutionGroup::new(group_id, started_at);
        group.tools.extend(activities);
        self.active_execution_group_id = Some(group_id);
        self.push_transcript_item(TranscriptItem::ExecutionGroup(group));
        self.recompute_active_tools();
    }
}

fn display_tool_arguments(arguments: &str, project_root: &std::path::Path) -> String {
    // Show tool paths under the project root as root-relative (`src/foo.rs`)
    // instead of the full absolute path. Display-only — the tool already ran
    // against the real path.
    shorten_path_args(arguments, project_root)
}

impl AppState {
    pub(super) fn close_active_execution_group(&mut self) {
        if let Some(group_id) = self.active_execution_group_id.take()
            && let Some(group) = self.execution_group_mut(group_id)
            && group.finished_at.is_none()
        {
            group.finished_at = Some(Instant::now());
        }
    }

    pub(super) fn close_active_execution_group_if_no_running_tools(&mut self) {
        if self.active_tools.is_empty() {
            self.close_active_execution_group();
        }
    }

    pub(super) fn move_execution_group_selection(
        &self,
        group_id: u64,
        selected_tool: usize,
        delta: i16,
    ) -> Option<usize> {
        let group = self.execution_group(group_id)?;
        if group.tools.is_empty() {
            return Some(0);
        }

        let count = group.tools.len() as i32;
        let current = selected_tool.clamp(0, group.tools.len().saturating_sub(1)) as i32;
        let next = (current + delta as i32).rem_euclid(count);
        Some(next as usize)
    }

    pub(crate) fn execution_group_tool(
        &self,
        group_id: u64,
        selected_tool: usize,
    ) -> Option<&ToolActivity> {
        self.execution_group(group_id)
            .and_then(|group| group.selected_tool(selected_tool))
    }

    pub(crate) fn selected_execution_group_tool(&self) -> Option<&ToolActivity> {
        let selection = self.active_group_tool_selection.as_ref()?;
        self.execution_group_tool(selection.group_id, selection.selected_tool)
    }

    pub(crate) fn execution_group_is_expanded(&self, group_id: u64) -> bool {
        !self.serenity_mode || self.expanded_execution_groups.contains(&group_id)
    }

    pub(crate) fn toggle_execution_group_expansion(&mut self, group_id: u64) {
        if !self.serenity_mode {
            self.set_execution_group_tool_selection(group_id, 0);
            return;
        }
        if !self.expanded_execution_groups.remove(&group_id) {
            self.expanded_execution_groups.insert(group_id);
        }
        self.active_group_tool_selection = None;
        if let Some(index) = self.execution_group_index(group_id) {
            self.focus = Focus::Transcript;
            self.set_transcript_focus(index);
        }
    }

    pub(super) fn set_execution_group_tool_selection(
        &mut self,
        group_id: u64,
        selected_tool: usize,
    ) {
        let Some(group) = self.execution_group(group_id) else {
            return;
        };
        if group.tools.is_empty() {
            return;
        }
        let selected_tool = selected_tool.min(group.tools.len().saturating_sub(1));
        self.active_group_tool_selection = Some(InlineToolSelection {
            group_id,
            selected_tool,
        });
        if let Some(index) = self.execution_group_index(group_id) {
            self.focus = Focus::Transcript;
            self.set_transcript_focus(index);
        }
    }
}

/// Rewrite absolute `path`/`file_path` argument values that live under `root` to
/// a root-relative form, so a tool card reads `src/foo.rs` instead of the full
/// `/Users/.../project/src/foo.rs`. Display-only (the tool already ran against
/// the real path); leaves relative paths, paths outside the tree, non-string
/// values, and non-object/unparseable arguments untouched. An empty `root`
/// (unset, e.g. in tests) is a no-op.
fn shorten_path_args(arguments: &str, root: &std::path::Path) -> String {
    if root.as_os_str().is_empty() {
        return arguments.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.to_string();
    };
    let Some(map) = value.as_object_mut() else {
        return arguments.to_string();
    };
    let mut changed = false;
    for key in ["path", "file_path"] {
        let Some(text) = map.get(key).and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Ok(relative) = std::path::Path::new(text).strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().into_owned();
        if relative.is_empty() {
            continue;
        }
        map.insert(key.to_string(), serde_json::Value::String(relative));
        changed = true;
    }
    if changed {
        value.to_string()
    } else {
        arguments.to_string()
    }
}

#[cfg(test)]
mod shorten_path_tests {
    use super::shorten_path_args;
    use std::path::Path;

    #[test]
    fn rewrites_paths_under_root_to_relative() {
        let root = Path::new("/Users/wojtek/code/bonsai");
        let out = shorten_path_args(
            r#"{"path":"/Users/wojtek/code/bonsai/src/sandbox/mod.rs"}"#,
            root,
        );
        assert_eq!(out, r#"{"path":"src/sandbox/mod.rs"}"#);

        // `file_path` is shortened too, and other keys are preserved.
        let out = shorten_path_args(
            r#"{"file_path":"/Users/wojtek/code/bonsai/src/main.rs","limit":40}"#,
            root,
        );
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["file_path"], "src/main.rs");
        assert_eq!(value["limit"], 40);
    }

    #[test]
    fn leaves_outside_and_relative_and_unset_untouched() {
        let root = Path::new("/Users/wojtek/code/bonsai");
        // Outside the project tree — unchanged.
        let outside = r#"{"path":"/etc/hosts"}"#;
        assert_eq!(shorten_path_args(outside, root), outside);
        // Already relative — unchanged.
        let relative = r#"{"path":"src/main.rs"}"#;
        assert_eq!(shorten_path_args(relative, root), relative);
        // Empty root (unset) — no-op even for an under-root path.
        let abs = r#"{"path":"/Users/wojtek/code/bonsai/src/main.rs"}"#;
        assert_eq!(shorten_path_args(abs, Path::new("")), abs);
        // A bash command is not a path key — left verbatim.
        let cmd = r#"{"command":"cargo test"}"#;
        assert_eq!(shorten_path_args(cmd, root), cmd);
    }
}
