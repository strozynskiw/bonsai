//! Markdown rendering for plan documents.

use super::{FINDING_NO_FILE, PlanDoc, flatten_ws};

impl PlanDoc {
    /// Serializes the canvas to markdown for rendering and `/export`, including
    /// the `## Findings` block.
    pub fn to_markdown(&self) -> String {
        self.to_markdown_inner(true, true)
    }

    /// Plan markdown for the `/start` hand-off. Omits the `## Findings` block:
    /// the coding agent gets findings as a dedicated structured section, so
    /// they are not emitted twice in one prompt.
    pub(crate) fn to_markdown_for_handoff(&self) -> String {
        self.to_markdown_inner(true, false)
    }

    fn to_markdown_inner(&self, include_title: bool, include_findings: bool) -> String {
        let mut out = String::new();
        if include_title && !self.title.is_empty() {
            out.push_str(&format!("# {}\n\n", self.title));
        }
        for section in &self.sections {
            out.push_str(&format!("## {}\n\n", section.heading));
            let body = section.body.trim_end();
            if !body.is_empty() {
                out.push_str(body);
                out.push_str("\n\n");
            }
        }
        self.push_questions_markdown(&mut out);
        self.push_phases_markdown(&mut out);
        self.push_tasks_markdown(&mut out);
        if include_findings {
            self.push_findings_markdown(&mut out);
        }
        out.trim_end().to_string()
    }

    fn push_findings_markdown(&self, out: &mut String) {
        if self.findings.is_empty() {
            return;
        }
        out.push_str("## Findings\n\n");
        for finding in self.findings_in_severity_order() {
            let status = if finding.resolved { " (resolved)" } else { "" };
            out.push_str(&format!(
                "- **[{}]**{} {} — {}\n",
                finding.severity.label(),
                status,
                finding
                    .location_label()
                    .unwrap_or_else(|| FINDING_NO_FILE.to_string()),
                flatten_ws(&finding.issue)
            ));
            if let Some(task) = &finding.task {
                out.push_str(&format!("  - Task: {task}\n"));
            }
        }
        out.push('\n');
    }

    fn push_questions_markdown(&self, out: &mut String) {
        if self.questions.is_empty() {
            return;
        }
        out.push_str("## Open questions\n\n");
        for question in &self.questions {
            out.push_str(&format!("- {question}\n"));
        }
        out.push('\n');
    }

    fn push_phases_markdown(&self, out: &mut String) {
        for phase in &self.phases {
            out.push_str(&format!("## {}\n\n", phase.name));
            for task in &phase.tasks {
                let mark = if task.done { 'x' } else { ' ' };
                out.push_str(&format!("- [{mark}] {}\n", task.text));
            }
            out.push('\n');
        }
    }

    fn push_tasks_markdown(&self, out: &mut String) {
        if self.tasks.is_empty() {
            return;
        }
        out.push_str("## Tasks\n\n");
        for task in &self.tasks {
            let mark = if task.done { 'x' } else { ' ' };
            out.push_str(&format!("- [{mark}] {}\n", task.text));
        }
    }

    /// Canvas-body markdown without the title heading.
    #[cfg(test)]
    pub fn to_markdown_body(&self) -> String {
        self.to_markdown_inner(false, true)
    }
}
