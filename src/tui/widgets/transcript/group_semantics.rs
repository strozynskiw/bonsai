use std::collections::BTreeSet;
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::tui::app::ToolStatus;
use crate::tui::app::{ExecutionGroup, ToolActivity};

use super::{
    CommandWorkflow, bash_command_from_args, classify_command_workflow, compact_text,
    meaningful_duration, primary_arg_value, tool_activity_display_name,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionPhase {
    Inspect,
    Edit,
    Verify,
    Recover,
    Summarize,
}

impl ExecutionPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Inspect => "Inspect",
            Self::Edit => "Edit",
            Self::Verify => "Verify",
            Self::Recover => "Recover",
            Self::Summarize => "Summarize",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExecutionGroupSemantics {
    phase: ExecutionPhase,
    total: usize,
    succeeded: usize,
    failed: usize,
    running: usize,
    read_files: BTreeSet<String>,
    edited_files: BTreeSet<String>,
    command_labels: Vec<String>,
    added_lines: usize,
    removed_lines: usize,
    duration: Option<String>,
}

impl ExecutionGroupSemantics {
    pub(super) fn summary(&self, hidden_tools: usize) -> String {
        let mut parts = Vec::new();
        parts.push(self.phase.label().to_string());
        parts.push(format!(
            "{} tool{}",
            self.total,
            if self.total == 1 { "" } else { "s" }
        ));

        if self.running > 0 {
            parts.push(format!("{} running", self.running));
        }
        if self.failed > 0 {
            parts.push(format!("{} ok / {} failed", self.succeeded, self.failed));
        }
        if !self.read_files.is_empty() {
            parts.push(file_summary("read", &self.read_files));
        }
        if !self.edited_files.is_empty() {
            parts.push(file_summary("edited", &self.edited_files));
        }
        if !self.command_labels.is_empty() {
            parts.push(command_summary(&self.command_labels));
        }
        if self.added_lines > 0 || self.removed_lines > 0 {
            parts.push(format!("+{} -{}", self.added_lines, self.removed_lines));
        }
        if hidden_tools > 0 {
            parts.push(format!(
                "+{} more {}",
                hidden_tools,
                if hidden_tools == 1 { "tool" } else { "tools" }
            ));
        }
        if let Some(duration) = &self.duration {
            parts.push(duration.clone());
        }
        parts.join(" · ")
    }
}

pub(super) fn summarize_group(group: &ExecutionGroup) -> ExecutionGroupSemantics {
    let (succeeded, failed, running, total) = group.tool_counts();
    let mut read_files = BTreeSet::new();
    let mut edited_files = BTreeSet::new();
    let mut command_labels = Vec::new();
    let mut has_edit = false;
    let mut has_verify = false;
    let mut all_summary = !group.tools.is_empty();
    let mut added_lines = 0usize;
    let mut removed_lines = 0usize;

    for activity in &group.tools {
        let class = classify_activity(activity);
        has_edit |= class.has_edit;
        has_verify |= class.has_verify;
        all_summary &= class.is_summary;

        if activity.name == "read"
            && let Some(path) = path_arg(&activity.arguments)
        {
            read_files.insert(path);
        }
        if let Some(diff) = &activity.diff {
            for file in diff.files() {
                edited_files.insert(file.path.clone());
            }
            let stats = diff.stats();
            added_lines = added_lines.saturating_add(stats.added);
            removed_lines = removed_lines.saturating_add(stats.removed);
        } else if matches!(activity.name.as_str(), "edit" | "write")
            && let Some(path) = path_arg(&activity.arguments)
        {
            edited_files.insert(path);
        }

        if activity.name == "bash" && bash_command_from_args(&activity.arguments).is_some() {
            push_unique(&mut command_labels, tool_activity_display_name(activity));
        }
    }

    let phase = if has_edit {
        ExecutionPhase::Edit
    } else if has_verify {
        ExecutionPhase::Verify
    } else if failed > 0 {
        ExecutionPhase::Recover
    } else if all_summary {
        ExecutionPhase::Summarize
    } else {
        ExecutionPhase::Inspect
    };

    ExecutionGroupSemantics {
        phase,
        total,
        succeeded,
        failed,
        running,
        read_files,
        edited_files,
        command_labels,
        added_lines,
        removed_lines,
        duration: group_duration(group).and_then(meaningful_duration),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivityClass {
    has_edit: bool,
    has_verify: bool,
    is_summary: bool,
}

fn classify_activity(activity: &ToolActivity) -> ActivityClass {
    let mut class = ActivityClass {
        has_edit: matches!(activity.name.as_str(), "edit" | "write") || activity.diff.is_some(),
        has_verify: matches!(activity.name.as_str(), "diagnostics"),
        is_summary: is_summary_tool(&activity.name),
    };

    if activity.name == "bash"
        && let Some(command) = bash_command_from_args(&activity.arguments)
        && let Some(workflow) = classify_command_workflow(&command)
    {
        class.has_verify = matches!(
            workflow,
            CommandWorkflow::Lint
                | CommandWorkflow::Test
                | CommandWorkflow::Build
                | CommandWorkflow::Check
                | CommandWorkflow::Git
        );
        class.has_edit =
            matches!(workflow, CommandWorkflow::Format) && !command.contains("--check");
    }

    class
}

fn is_summary_tool(name: &str) -> bool {
    name == "todo_write" || name == "set_session_title" || name.starts_with("plan_")
}

fn path_arg(arguments: &str) -> Option<String> {
    for key in ["file_path", "path"] {
        if let Some((found_key, value)) = primary_arg_value(arguments)
            && found_key == key
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

fn group_duration(group: &ExecutionGroup) -> Option<Duration> {
    let started_at = group
        .tools
        .iter()
        .map(|activity| activity.started_at)
        .min()?;
    let finished_at = group
        .tools
        .iter()
        .map(|activity| activity.finished_at.unwrap_or_else(Instant::now))
        .max()?;
    Some(finished_at.saturating_duration_since(started_at))
}

fn file_summary(verb: &str, files: &BTreeSet<String>) -> String {
    if files.len() == 1
        && let Some(path) = files.iter().next()
    {
        return format!("{verb} {}", compact_text(path, 48));
    }
    format!("{verb} {}", plural_count(files.len(), "file", "files"))
}

fn command_summary(labels: &[String]) -> String {
    let shown = labels.iter().take(2).cloned().collect::<Vec<_>>();
    let mut summary = format!("cmds: {}", shown.join(", "));
    let hidden = labels.len().saturating_sub(shown.len());
    if hidden > 0 {
        summary.push_str(&format!(" +{hidden}"));
    }
    summary
}

fn plural_count(count: usize, singular: &str, plural: &str) -> String {
    format!("{} {}", count, if count == 1 { singular } else { plural })
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_subsecond_group_duration() {
        let started_at = Instant::now();
        let group = ExecutionGroup {
            id: 1,
            finished_at: Some(started_at + Duration::from_millis(250)),
            tools: vec![ToolActivity {
                id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
                delegated_model: None,
                status: ToolStatus::Succeeded,
                result: Some("ok".to_string()),
                diff: None,
                started_at,
                finished_at: Some(started_at + Duration::from_millis(250)),
            }],
        };

        let summary = summarize_group(&group).summary(0);

        assert!(summary.contains("250ms"), "{summary}");
    }

    #[test]
    fn keeps_short_group_duration_quiet() {
        let started_at = Instant::now();
        let group = ExecutionGroup {
            id: 1,
            finished_at: Some(started_at + Duration::from_millis(20)),
            tools: vec![ToolActivity {
                id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
                delegated_model: None,
                status: ToolStatus::Succeeded,
                result: Some("ok".to_string()),
                diff: None,
                started_at,
                finished_at: Some(started_at + Duration::from_millis(20)),
            }],
        };

        let summary = summarize_group(&group).summary(0);

        assert!(!summary.contains("20ms"), "{summary}");
    }
}
