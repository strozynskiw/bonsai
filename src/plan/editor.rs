//! Failure-atomic plan mutation service.

use super::{Finding, InsertPosition, PlanDoc, SectionPatch};

/// Applies validated mutations to one plan document.
///
/// Every method is one command boundary: failed commands restore the prior
/// document and successful content changes advance the revision exactly once.
#[derive(Debug)]
pub struct PlanEditor<'a> {
    doc: &'a mut PlanDoc,
}

impl PlanDoc {
    /// Creates a mutation service for this document.
    pub fn edit(&mut self) -> PlanEditor<'_> {
        PlanEditor { doc: self }
    }
}

impl PlanEditor<'_> {
    /// Clears all user-visible plan content.
    pub fn clear(&mut self) {
        self.apply(PlanDoc::clear);
    }

    /// Sets the title after trimming it.
    #[cfg(test)]
    pub fn set_title(&mut self, title: &str) {
        self.apply(|doc| doc.set_title(title));
    }

    /// Sets a non-empty title.
    pub fn set_title_checked(&mut self, title: &str) -> anyhow::Result<()> {
        self.try_apply(|doc| doc.set_title_checked(title))
    }

    /// Creates or replaces a section.
    #[cfg(test)]
    pub fn set_section(&mut self, heading: &str, body: &str) {
        self.apply(|doc| doc.set_section(heading, body));
    }

    /// Creates or replaces a section with a non-empty heading.
    pub fn set_section_checked(&mut self, heading: &str, body: &str) -> anyhow::Result<()> {
        self.try_apply(|doc| doc.set_section_checked(heading, body))
    }

    /// Applies a typed patch to a section.
    pub fn patch_section(&mut self, heading: &str, patch: SectionPatch) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.patch_section(heading, patch))
    }

    /// Removes a section by heading.
    pub fn remove_section(&mut self, heading: &str) -> bool {
        self.apply(|doc| doc.remove_section(heading))
    }

    /// Moves a section relative to the plan's section order.
    pub fn move_section(
        &mut self,
        heading: &str,
        position: InsertPosition,
        target: Option<&str>,
    ) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.move_section(heading, position, target))
    }

    /// Adds a phase and its initial task list.
    pub fn add_phase_with_tasks_checked(
        &mut self,
        name: &str,
        tasks: &[String],
    ) -> anyhow::Result<usize> {
        self.try_apply(|doc| doc.add_phase_with_tasks_checked(name, tasks))
    }

    /// Adds an empty phase for focused model and rendering tests.
    #[cfg(test)]
    pub fn add_phase(&mut self, name: &str) {
        self.apply(|doc| doc.add_phase(name));
    }

    /// Appends one task to a phase for focused tests.
    #[cfg(test)]
    pub fn add_task_to_phase(&mut self, phase: &str, text: &str) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.add_task_to_phase(phase, text))
    }

    /// Removes a phase and its tasks.
    pub fn remove_phase(&mut self, name: &str) -> bool {
        self.apply(|doc| doc.remove_phase(name))
    }

    /// Moves a phase relative to the plan's phase order.
    pub fn move_phase(
        &mut self,
        name: &str,
        position: InsertPosition,
        target: Option<&str>,
    ) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.move_phase(name, position, target))
    }

    /// Inserts a task into a named phase.
    pub fn insert_task_to_phase(
        &mut self,
        phase: &str,
        text: &str,
        position: InsertPosition,
        target: Option<&str>,
    ) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.insert_task_to_phase(phase, text, position, target))
    }

    /// Adds a flat task when it does not already exist.
    #[cfg(test)]
    pub fn add_task(&mut self, text: &str) {
        self.apply(|doc| doc.add_task(text));
    }

    /// Adds a non-empty flat task.
    pub fn add_task_checked(&mut self, text: &str) -> anyhow::Result<()> {
        self.try_apply(|doc| doc.add_task_checked(text))
    }

    /// Adds an open question when it does not already exist.
    #[cfg(test)]
    pub fn add_question(&mut self, text: &str) {
        self.apply(|doc| doc.add_question(text));
    }

    /// Adds a non-empty open question.
    #[cfg(test)]
    pub fn add_question_checked(&mut self, text: &str) -> anyhow::Result<()> {
        self.try_apply(|doc| doc.add_question_checked(text))
    }

    /// Adds or refines a structured finding.
    #[cfg(test)]
    pub fn add_finding(&mut self, finding: Finding) {
        self.apply(|doc| doc.add_finding(finding));
    }

    /// Adds a finding with its required fields validated.
    pub fn add_finding_checked(&mut self, finding: Finding) -> anyhow::Result<()> {
        self.try_apply(|doc| doc.add_finding_checked(finding))
    }

    /// Associates a finding with a task.
    pub fn associate_finding(
        &mut self,
        finding_match: &str,
        task_match: &str,
    ) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.associate_finding(finding_match, task_match))
    }

    /// Marks a finding resolved without deleting its evidence.
    pub fn resolve_finding(&mut self, target: &str) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.resolve_finding(target))
    }

    /// Removes a matching open question.
    pub fn remove_question(&mut self, target: &str) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.remove_question(target))
    }

    /// Inserts a flat task at a typed position.
    pub fn insert_task(
        &mut self,
        text: &str,
        position: InsertPosition,
        target: Option<&str>,
    ) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.insert_task(text, position, target))
    }

    /// Updates a matching task's text.
    pub fn update_task(&mut self, target: &str, text: &str) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.update_task(target, text))
    }

    /// Removes a matching task.
    pub fn remove_task(&mut self, target: &str) -> anyhow::Result<String> {
        self.try_apply(|doc| doc.remove_task(target))
    }

    /// Marks the first matching open task complete.
    pub fn check_task(&mut self, target: &str) -> Option<String> {
        self.apply(|doc| doc.check_task(target))
    }

    /// Reopens the first matching completed task.
    pub fn uncheck_task(&mut self, target: &str) -> Option<String> {
        self.apply(|doc| doc.uncheck_task(target))
    }

    /// Marks all tasks in one phase complete.
    pub fn mark_phase_done(&mut self, phase_index: usize) {
        self.apply(|doc| doc.mark_phase_done(phase_index));
    }

    fn apply<T>(&mut self, operation: impl FnOnce(&mut PlanDoc) -> T) -> T {
        let before = self.doc.clone();
        let result = operation(self.doc);
        self.finish(&before);
        result
    }

    fn try_apply<T>(
        &mut self,
        operation: impl FnOnce(&mut PlanDoc) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let before = self.doc.clone();
        match operation(self.doc) {
            Ok(result) => {
                self.finish(&before);
                Ok(result)
            }
            Err(error) => {
                *self.doc = before;
                Err(error)
            }
        }
    }

    fn finish(&mut self, before: &PlanDoc) {
        self.doc.revision = if content_eq(self.doc, before) {
            before.revision
        } else {
            before.revision.saturating_add(1)
        };
    }
}

fn content_eq(left: &PlanDoc, right: &PlanDoc) -> bool {
    left.title == right.title
        && left.sections == right.sections
        && left.questions == right.questions
        && left.tasks == right.tasks
        && left.phases == right.phases
        && left.findings == right.findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_editor_command_advances_revision_once() {
        let mut doc = PlanDoc::default();
        doc.edit().set_section("Goal", "Ship it");
        assert_eq!(doc.revision, 1);

        doc.edit()
            .patch_section("Approach", SectionPatch::Replace("Build it".to_string()))
            .expect("replace patch should create the section");
        assert_eq!(
            doc.revision, 2,
            "nested raw mutations still form one editor command"
        );

        doc.edit().set_section("Goal", "Ship it");
        assert_eq!(doc.revision, 2, "an idempotent command is not a mutation");
    }

    #[test]
    fn failed_editor_command_restores_document_and_revision() {
        let mut doc = PlanDoc::default();
        doc.edit().set_section("Goal", "Ship it");
        let before = doc.clone();

        let error = doc
            .edit()
            .move_section("Goal", InsertPosition::Before, Some("Missing"))
            .expect_err("missing target must fail");

        assert!(error.to_string().contains("No plan section"));
        assert_eq!(doc, before);
    }
}
