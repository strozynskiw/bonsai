//! Read-only projections from plan documents into runtime todo items.

use crate::todo::{TodoItem, TodoStatus};

use super::{FINDING_NO_FILE, FINDING_UNASSIGNED, PlanDoc, PlanTask, flatten_ws};

impl PlanDoc {
    /// Renders unresolved findings as the deterministic `/start` handoff block.
    pub fn findings_handoff_block(&self) -> String {
        let findings: Vec<_> = self
            .query()
            .findings_in_severity_order()
            .into_iter()
            .filter(|finding| !finding.resolved)
            .collect();
        if findings.is_empty() {
            return String::new();
        }

        let mut out = String::from("## Review findings (must address)\n");
        for finding in findings {
            out.push_str(&format!(
                "[{}] {} — {}\n",
                finding.severity.label(),
                finding
                    .location_label()
                    .unwrap_or_else(|| FINDING_NO_FILE.to_string()),
                flatten_ws(&finding.issue)
            ));
            out.push_str(&format!(
                "  Required fix: {}\n",
                flatten_ws(&finding.required_fix)
            ));
            if !finding.acceptance_tests.is_empty() {
                let acceptance = finding
                    .acceptance_tests
                    .iter()
                    .map(|test| flatten_ws(test))
                    .collect::<Vec<_>>()
                    .join("; ");
                out.push_str(&format!("  Acceptance: {acceptance}\n"));
            }
            out.push_str(&format!(
                "  Task: {}\n",
                finding.task.as_deref().unwrap_or(FINDING_UNASSIGNED)
            ));
            if !finding.source_ids.is_empty() {
                out.push_str(&format!("  Evidence: {}\n", finding.source_ids.join(", ")));
            }
        }
        out.trim_end().to_string()
    }

    /// Maps the visible flat checklist to agent todo items.
    ///
    /// Completed tasks remain completed, the first open task becomes the sole
    /// in-progress item, and remaining open tasks stay pending. When the
    /// structured task list is empty, task-like section Markdown is used.
    pub fn tasks_as_todo_items(&self) -> Vec<TodoItem> {
        let markdown_tasks;
        let tasks = if self.tasks.is_empty() {
            markdown_tasks = self.markdown_task_items();
            markdown_tasks.as_slice()
        } else {
            self.tasks.as_slice()
        };
        tasks_to_todo_items(tasks)
    }

    /// Maps one phase checklist to todo items using the flat-plan semantics.
    pub fn phase_todo_items(&self, phase_index: usize) -> Vec<TodoItem> {
        self.phases
            .get(phase_index)
            .map(|phase| tasks_to_todo_items(&phase.tasks))
            .unwrap_or_default()
    }

    /// Returns the next phase that contains an unfinished task.
    pub fn next_phase_with_pending(&self, after: Option<usize>) -> Option<usize> {
        let start = after.map_or(0, |index| index + 1);
        (start..self.phases.len()).find(|&index| self.phases[index].tasks.iter().any(|t| !t.done))
    }

    fn markdown_task_items(&self) -> Vec<PlanTask> {
        let mut tasks = Vec::new();
        for section in &self.sections {
            let taskish_section = is_taskish_heading(&section.heading);
            for line in section.body.lines() {
                let task = parse_markdown_checkbox_task(line).or_else(|| {
                    taskish_section
                        .then(|| parse_markdown_bullet_task(line))
                        .flatten()
                });
                if let Some(task) = task
                    && !tasks
                        .iter()
                        .any(|existing: &PlanTask| existing.text == task.text)
                {
                    tasks.push(task);
                }
            }
        }
        tasks
    }
}

fn tasks_to_todo_items(tasks: &[PlanTask]) -> Vec<TodoItem> {
    let mut out = Vec::with_capacity(tasks.len());
    let mut in_progress_assigned = false;
    for task in tasks {
        let status = if task.done {
            TodoStatus::Completed
        } else if !in_progress_assigned {
            in_progress_assigned = true;
            TodoStatus::InProgress
        } else {
            TodoStatus::Pending
        };
        out.push(TodoItem {
            content: task.text.clone(),
            status,
        });
    }
    out
}

fn parse_markdown_checkbox_task(line: &str) -> Option<PlanTask> {
    let rest = strip_markdown_list_marker(line)?.trim_start();
    let (done, text) = if let Some(text) = rest.strip_prefix("[ ]") {
        (false, text)
    } else if let Some(text) = rest.strip_prefix("[x]") {
        (true, text)
    } else {
        (true, rest.strip_prefix("[X]")?)
    };
    let text = text.trim_start();
    (!text.is_empty()).then(|| PlanTask {
        text: text.to_string(),
        done,
    })
}

fn parse_markdown_bullet_task(line: &str) -> Option<PlanTask> {
    let text = strip_markdown_list_marker(line)?.trim_start();
    if text.is_empty() || text.starts_with('[') {
        return None;
    }
    Some(PlanTask {
        text: text.to_string(),
        done: false,
    })
}

fn strip_markdown_list_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest);
        }
    }

    let digit_count = trimmed
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_count == 0 {
        return None;
    }
    let rest = &trimmed[digit_count..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
}

fn is_taskish_heading(heading: &str) -> bool {
    let lower = heading.to_lowercase();
    ["task", "todo", "checklist", "implementation", "step"]
        .iter()
        .any(|needle| lower.contains(needle))
}
