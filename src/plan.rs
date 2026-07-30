//! Plan-mode document domain and editing services.

pub mod editor;
mod markdown;
mod model;
mod projection;
pub mod query;

pub use model::{
    Finding, InsertPosition, PlanDoc, PlanPhase, PlanSection, PlanTask, SectionPatch, Severity,
    SharedPlanStore,
};
impl PlanDoc {
    fn clear(&mut self) {
        self.title.clear();
        self.sections.clear();
        self.questions.clear();
        self.tasks.clear();
        self.phases.clear();
        self.findings.clear();
        self.revision += 1;
    }

    fn set_title(&mut self, title: &str) {
        self.title = title.trim().to_string();
        self.revision += 1;
    }

    /// Validates and sets the title, rejecting blank input. The empty-input
    /// invariant lives here in the model so every entry point shares it; the
    /// `plan_set_title` tool relies on this instead of validating itself.
    fn set_title_checked(&mut self, title: &str) -> anyhow::Result<()> {
        if title.trim().is_empty() {
            anyhow::bail!("title must not be empty");
        }
        self.set_title(title);
        Ok(())
    }

    /// Creates or replaces a section, matched case-insensitively by heading.
    /// New sections append in arrival order so the plan reads top to bottom.
    fn set_section(&mut self, heading: &str, body: &str) {
        let heading = heading.trim();
        let body = body.trim().to_string();
        if let Some(section) = self.section_mut(heading) {
            section.heading = heading.to_string();
            section.body = body;
        } else {
            self.sections.push(PlanSection {
                heading: heading.to_string(),
                body,
            });
        }
        self.revision += 1;
    }

    /// Validates and sets a section, rejecting a blank heading. Centralizes the
    /// empty-input invariant in the model so the `plan_set_section` tool does
    /// not have to duplicate it.
    fn set_section_checked(&mut self, heading: &str, body: &str) -> anyhow::Result<()> {
        if heading.trim().is_empty() {
            anyhow::bail!("heading must not be empty");
        }
        self.set_section(heading, body);
        Ok(())
    }

    /// Patch a section in place, matched case-insensitively by heading.
    /// Missing sections are created for replace/append/prepend operations.
    /// The operands required by each operation are guaranteed present by the
    /// `SectionPatch` enum, so this only validates the heading.
    fn patch_section(&mut self, heading: &str, patch: SectionPatch) -> anyhow::Result<String> {
        let heading = heading.trim();
        if heading.is_empty() {
            anyhow::bail!("heading must not be empty");
        }

        match patch {
            SectionPatch::Replace(text) => {
                self.set_section(heading, &text);
                Ok(format!("Plan section '{heading}' replaced."))
            }
            SectionPatch::Append(text) => {
                let section = self.ensure_section(heading);
                append_block(&mut section.body, &text);
                self.revision += 1;
                Ok(format!("Appended to plan section '{heading}'."))
            }
            SectionPatch::Prepend(text) => {
                let section = self.ensure_section(heading);
                prepend_block(&mut section.body, &text);
                self.revision += 1;
                Ok(format!("Prepended to plan section '{heading}'."))
            }
            SectionPatch::ReplaceText { old, new } => {
                let section = self
                    .section_mut(heading)
                    .ok_or_else(|| anyhow::anyhow!("No plan section named '{heading}' found."))?;
                if !section.body.contains(&old) {
                    anyhow::bail!("Section '{heading}' does not contain old_text: {old:?}");
                }
                section.body = section.body.replacen(&old, &new, 1);
                self.revision += 1;
                Ok(format!("Patched text in plan section '{heading}'."))
            }
        }
    }

    /// Removes a section by heading. Returns whether anything was removed.
    fn remove_section(&mut self, heading: &str) -> bool {
        let target = heading.trim().to_lowercase();
        let before = self.sections.len();
        self.sections
            .retain(|section| section.heading.to_lowercase() != target);
        let removed = self.sections.len() != before;
        if removed {
            self.revision += 1;
        }
        removed
    }

    /// Reorders an existing section, matched case-insensitively by heading,
    /// using the same `start | end | before | after` vocabulary as task
    /// inserts. The destination is resolved against the list *after* the moved
    /// section is removed, so relative targets never land off-by-one; on a bad
    /// target the section is restored to its original slot rather than dropped.
    fn move_section(
        &mut self,
        heading: &str,
        position: InsertPosition,
        target: Option<&str>,
    ) -> anyhow::Result<String> {
        let heading = heading.trim();
        let from = self
            .query()
            .section_index(heading)
            .ok_or_else(|| anyhow::anyhow!("No plan section named '{heading}' found."))?;
        let section = self.sections.remove(from);

        let index = match position {
            InsertPosition::Start => Ok(0),
            InsertPosition::End => Ok(self.sections.len()),
            InsertPosition::Before | InsertPosition::After => target
                .map(str::trim)
                .filter(|target| !target.is_empty())
                .ok_or_else(|| anyhow::anyhow!("target is required for before/after positions"))
                .and_then(|target| {
                    self.query()
                        .section_index(target)
                        .ok_or_else(|| anyhow::anyhow!("No plan section named '{target}' found."))
                        .map(|matched| {
                            if matches!(position, InsertPosition::After) {
                                matched + 1
                            } else {
                                matched
                            }
                        })
                }),
        };

        match index {
            Ok(index) => {
                self.sections
                    .insert(index.min(self.sections.len()), section);
                self.revision += 1;
                Ok(format!("Plan section '{heading}' moved."))
            }
            Err(error) => {
                // Restore the section so a missing target never loses content.
                self.sections.insert(from.min(self.sections.len()), section);
                Err(error)
            }
        }
    }

    /// Appends a phase unless one with the same name (case-insensitive)
    /// already exists. New phases append in arrival order.
    #[cfg(test)]
    fn add_phase(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() || self.query().phase_index(name).is_some() {
            return;
        }
        self.phases.push(PlanPhase {
            name: name.to_string(),
            tasks: Vec::new(),
        });
        self.revision += 1;
    }

    /// Validates and appends a phase with its initial task checklist in one
    /// mutation. Duplicate task labels inside the provided list are collapsed in
    /// first-seen order, matching the normal task de-dupe behavior.
    fn add_phase_with_tasks_checked(
        &mut self,
        name: &str,
        tasks: &[String],
    ) -> anyhow::Result<usize> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("phase name must not be empty");
        }
        if self.query().phase_index(name).is_some() {
            anyhow::bail!("a phase named '{name}' already exists");
        }

        let mut phase_tasks = Vec::new();
        for task in tasks {
            let task = task.trim();
            if task.is_empty() {
                anyhow::bail!("phase task text must not be empty");
            }
            if !phase_tasks
                .iter()
                .any(|existing: &PlanTask| existing.text == task)
            {
                phase_tasks.push(PlanTask {
                    text: task.to_string(),
                    done: false,
                });
            }
        }

        let task_count = phase_tasks.len();
        self.phases.push(PlanPhase {
            name: name.to_string(),
            tasks: phase_tasks,
        });
        self.revision += 1;
        Ok(task_count)
    }

    /// Removes a phase by name (case-insensitive), along with its tasks.
    /// Returns whether anything was removed.
    fn remove_phase(&mut self, name: &str) -> bool {
        let target = name.trim().to_lowercase();
        let before = self.phases.len();
        self.phases
            .retain(|phase| phase.name.to_lowercase() != target);
        let removed = self.phases.len() != before;
        if removed {
            self.revision += 1;
        }
        removed
    }

    /// Reorders a phase by name, using the same `start | end | before | after`
    /// vocabulary as section moves. Mirrors `move_section`: the destination is
    /// resolved after the moved phase is removed, and a bad target restores the
    /// phase to its original slot rather than dropping it.
    fn move_phase(
        &mut self,
        name: &str,
        position: InsertPosition,
        target: Option<&str>,
    ) -> anyhow::Result<String> {
        let name = name.trim();
        let from = self
            .query()
            .phase_index(name)
            .ok_or_else(|| anyhow::anyhow!("No plan phase named '{name}' found."))?;
        let phase = self.phases.remove(from);

        let index = match position {
            InsertPosition::Start => Ok(0),
            InsertPosition::End => Ok(self.phases.len()),
            InsertPosition::Before | InsertPosition::After => target
                .map(str::trim)
                .filter(|target| !target.is_empty())
                .ok_or_else(|| anyhow::anyhow!("target is required for before/after positions"))
                .and_then(|target| {
                    self.query()
                        .phase_index(target)
                        .ok_or_else(|| anyhow::anyhow!("No plan phase named '{target}' found."))
                        .map(|matched| {
                            if matches!(position, InsertPosition::After) {
                                matched + 1
                            } else {
                                matched
                            }
                        })
                }),
        };

        match index {
            Ok(index) => {
                self.phases.insert(index.min(self.phases.len()), phase);
                self.revision += 1;
                Ok(format!("Plan phase '{name}' moved."))
            }
            Err(error) => {
                self.phases.insert(from.min(self.phases.len()), phase);
                Err(error)
            }
        }
    }

    /// Appends a task to the named phase unless an identical one already exists
    /// in it. The phase must already exist.
    #[cfg(test)]
    fn add_task_to_phase(&mut self, phase: &str, text: &str) -> anyhow::Result<String> {
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("task text must not be empty");
        }
        let index = self
            .query()
            .phase_index(phase)
            .ok_or_else(|| anyhow::anyhow!("No plan phase named '{}' found.", phase.trim()))?;
        let tasks = &mut self.phases[index].tasks;
        if tasks.iter().any(|task| task.text == text) {
            return Ok(format!(
                "Task already exists in phase ({} tasks).",
                tasks.len()
            ));
        }
        tasks.push(PlanTask {
            text: text.to_string(),
            done: false,
        });
        let count = self.phases[index].tasks.len();
        self.revision += 1;
        Ok(format!("Task added to phase ({count} tasks)."))
    }

    /// Inserts a task into the named phase at a position relative to that
    /// phase's own task list. The phase must already exist.
    fn insert_task_to_phase(
        &mut self,
        phase: &str,
        text: &str,
        position: InsertPosition,
        target: Option<&str>,
    ) -> anyhow::Result<String> {
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("task text must not be empty");
        }
        let phase_index = self
            .query()
            .phase_index(phase)
            .ok_or_else(|| anyhow::anyhow!("No plan phase named '{}' found.", phase.trim()))?;
        let tasks = &self.phases[phase_index].tasks;
        if tasks.iter().any(|task| task.text == text) {
            return Ok(format!(
                "Task already exists in phase ({} tasks).",
                tasks.len()
            ));
        }

        let index = match position {
            InsertPosition::Start => 0,
            InsertPosition::End => tasks.len(),
            InsertPosition::Before | InsertPosition::After => {
                let target = target
                    .map(str::trim)
                    .filter(|target| !target.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("target is required for before/after positions")
                    })?;
                let needle = target.to_lowercase();
                let matched = tasks
                    .iter()
                    .position(|task| task.text.to_lowercase().contains(&needle))
                    .ok_or_else(|| anyhow::anyhow!("No task matching '{target}' found."))?;
                if matches!(position, InsertPosition::After) {
                    matched + 1
                } else {
                    matched
                }
            }
        };

        self.phases[phase_index].tasks.insert(
            index,
            PlanTask {
                text: text.to_string(),
                done: false,
            },
        );
        let count = self.phases[phase_index].tasks.len();
        self.revision += 1;
        Ok(format!("Task inserted into phase ({count} tasks)."))
    }

    /// Appends a task unless an identical one already exists.
    fn add_task(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || self.tasks.iter().any(|task| task.text == text) {
            return;
        }
        self.tasks.push(PlanTask {
            text: text.to_string(),
            done: false,
        });
        self.revision += 1;
    }

    /// Validates and appends a task, rejecting blank input. `add_task` itself
    /// silently ignores empties; this surfaces that as an error so the
    /// `plan_add_task` tool gets an unambiguous signal from the model.
    fn add_task_checked(&mut self, text: &str) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            anyhow::bail!("task text must not be empty");
        }
        self.add_task(text);
        Ok(())
    }

    /// Appends an open question unless an identical one already exists.
    #[cfg(test)]
    fn add_question(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || self.questions.iter().any(|question| question == text) {
            return;
        }
        self.questions.push(text.to_string());
        self.revision += 1;
    }

    /// Validates and appends an open question, rejecting blank input.
    #[cfg(test)]
    fn add_question_checked(&mut self, text: &str) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            anyhow::bail!("question text must not be empty");
        }
        self.add_question(text);
        Ok(())
    }

    /// Records a finding. A re-record with the same identity (severity, file,
    /// line, issue text) is treated as a *refinement*: the existing entry's
    /// required fix, acceptance tests, evidence, and task link are updated in
    /// place rather than dropped — so a corrected finding is never silently
    /// lost. A genuinely new finding is appended.
    fn add_finding(&mut self, finding: Finding) {
        if let Some(existing) = self.findings.iter_mut().find(|existing| {
            existing.severity == finding.severity
                && existing.file == finding.file
                && existing.line == finding.line
                && existing.issue.trim() == finding.issue.trim()
        }) {
            existing.required_fix = finding.required_fix;
            existing.acceptance_tests = finding.acceptance_tests;
            existing.source_ids = finding.source_ids;
            if finding.task.is_some() {
                existing.task = finding.task;
            }
            self.revision += 1;
            return;
        }
        self.findings.push(finding);
        self.revision += 1;
    }

    /// Validates required fields then appends, so `plan_add_finding` gets an
    /// unambiguous signal from the model.
    fn add_finding_checked(&mut self, finding: Finding) -> anyhow::Result<()> {
        if finding.issue.trim().is_empty() {
            anyhow::bail!("finding issue must not be empty");
        }
        if finding.required_fix.trim().is_empty() {
            anyhow::bail!("finding required_fix must not be empty");
        }
        self.add_finding(finding);
        Ok(())
    }

    /// Links the first finding whose issue matches `finding_match` to the task
    /// whose text contains `task_match` (both case-insensitive). Association is
    /// by task *text* — editing that task's text later breaks the link, the same
    /// fragility every text-matched plan op (`check_task`, …) already accepts.
    fn associate_finding(
        &mut self,
        finding_match: &str,
        task_match: &str,
    ) -> anyhow::Result<String> {
        let task_text = match self
            .query()
            .task_location(task_match)
            .ok_or_else(|| anyhow::anyhow!("No plan task matching {task_match:?} found."))?
        {
            (None, index) => self.tasks[index].text.clone(),
            (Some(phase), index) => self.phases[phase].tasks[index].text.clone(),
        };
        let finding_index = self.query().finding_index(finding_match)?;
        let finding = &mut self.findings[finding_index];
        finding.task = Some(task_text.clone());
        self.revision += 1;
        Ok(format!("Finding linked to task: {task_text}"))
    }

    /// Marks the first finding whose issue matches `target` resolved. Findings
    /// are never hard-deleted by the model — resolving keeps them on the record
    /// so nothing is silently lost.
    fn resolve_finding(&mut self, target: &str) -> anyhow::Result<String> {
        let finding_index = self.query().finding_index(target)?;
        let finding = &mut self.findings[finding_index];
        finding.resolved = true;
        let issue = finding.issue.clone();
        self.revision += 1;
        Ok(format!("Finding marked resolved: {issue}"))
    }

    /// Removes the first open question whose text contains `target`
    /// (case-insensitive). Returns the removed question text.
    fn remove_question(&mut self, target: &str) -> anyhow::Result<String> {
        let target = target.trim();
        if target.is_empty() {
            anyhow::bail!("target must not be empty");
        }
        let needle = target.to_lowercase();
        let index = self
            .questions
            .iter()
            .position(|question| question.to_lowercase().contains(&needle))
            .ok_or_else(|| anyhow::anyhow!("No open question matching '{target}' found."))?;
        let removed = self.questions.remove(index);
        self.revision += 1;
        Ok(format!("Open question removed: {removed}"))
    }

    fn insert_task(
        &mut self,
        text: &str,
        position: InsertPosition,
        target: Option<&str>,
    ) -> anyhow::Result<String> {
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("task text must not be empty");
        }
        if self.tasks.iter().any(|task| task.text == text) {
            return Ok(format!(
                "Task already exists ({} tasks total).",
                self.tasks.len()
            ));
        }

        let index = match position {
            InsertPosition::Start => 0,
            InsertPosition::End => self.tasks.len(),
            InsertPosition::Before | InsertPosition::After => {
                let target = target
                    .map(str::trim)
                    .filter(|target| !target.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("target is required for before/after positions")
                    })?;
                let matched = self
                    .query()
                    .flat_task_index(target)
                    .ok_or_else(|| anyhow::anyhow!("No task matching '{target}' found."))?;
                if matches!(position, InsertPosition::After) {
                    matched + 1
                } else {
                    matched
                }
            }
        };

        self.tasks.insert(
            index,
            PlanTask {
                text: text.to_string(),
                done: false,
            },
        );
        self.revision += 1;
        Ok(format!("Task inserted ({} tasks total).", self.tasks.len()))
    }

    fn update_task(&mut self, target: &str, text: &str) -> anyhow::Result<String> {
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("task text must not be empty");
        }
        let target = target.trim();
        if target.is_empty() {
            anyhow::bail!("target must not be empty");
        }
        let (phase, index) = self
            .query()
            .task_location(target)
            .ok_or_else(|| anyhow::anyhow!("No task matching '{target}' found."))?;
        self.task_list_mut(phase)[index].text = text.to_string();
        self.revision += 1;
        Ok(format!("Task updated: {text}"))
    }

    fn remove_task(&mut self, target: &str) -> anyhow::Result<String> {
        let target = target.trim();
        if target.is_empty() {
            anyhow::bail!("target must not be empty");
        }
        let (phase, index) = self
            .query()
            .task_location(target)
            .ok_or_else(|| anyhow::anyhow!("No task matching '{target}' found."))?;
        let removed = self.task_list_mut(phase).remove(index);
        self.revision += 1;
        Ok(format!("Task removed: {}", removed.text))
    }

    /// Marks the first task whose text contains `needle` (case-insensitive)
    /// as done, searching the flat list first and then each phase in order.
    /// Returns the matched task text.
    fn check_task(&mut self, needle: &str) -> Option<String> {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let task = self
            .all_tasks_mut()
            .find(|task| !task.done && task.text.to_lowercase().contains(&needle))?;
        task.done = true;
        let text = task.text.clone();
        self.revision += 1;
        Some(text)
    }

    fn uncheck_task(&mut self, target: &str) -> Option<String> {
        let target = target.trim().to_lowercase();
        if target.is_empty() {
            return None;
        }
        let task = self
            .all_tasks_mut()
            .find(|task| task.done && task.text.to_lowercase().contains(&target))?;
        task.done = false;
        let text = task.text.clone();
        self.revision += 1;
        Some(text)
    }

    fn section_mut(&mut self, heading: &str) -> Option<&mut PlanSection> {
        let target = heading.to_lowercase();
        self.sections
            .iter_mut()
            .find(|section| section.heading.to_lowercase() == target)
    }

    fn ensure_section(&mut self, heading: &str) -> &mut PlanSection {
        if let Some(index) = self
            .sections
            .iter()
            .position(|section| section.heading.eq_ignore_ascii_case(heading))
        {
            self.sections[index].heading = heading.to_string();
            return &mut self.sections[index];
        }
        self.sections.push(PlanSection {
            heading: heading.to_string(),
            body: String::new(),
        });
        let index = self.sections.len() - 1;
        &mut self.sections[index]
    }

    /// Iterates every task in the plan — the flat list first, then each phase's
    /// tasks in order — so matching operations behave the same regardless of
    /// whether the plan is flat or phased.
    fn all_tasks_mut(&mut self) -> impl Iterator<Item = &mut PlanTask> {
        self.tasks.iter_mut().chain(
            self.phases
                .iter_mut()
                .flat_map(|phase| phase.tasks.iter_mut()),
        )
    }

    fn task_list_mut(&mut self, phase: Option<usize>) -> &mut Vec<PlanTask> {
        match phase {
            None => &mut self.tasks,
            Some(index) => &mut self.phases[index].tasks,
        }
    }

    /// Marks every task in a phase as done. Called when a phase's
    /// implementation run completes so the canvas and persistence reflect
    /// progress before auto-advancing.
    fn mark_phase_done(&mut self, phase_index: usize) {
        if let Some(phase) = self.phases.get_mut(phase_index) {
            let mut changed = false;
            for task in &mut phase.tasks {
                if !task.done {
                    task.done = true;
                    changed = true;
                }
            }
            if changed {
                self.revision += 1;
            }
        }
    }
}

/// Placeholder shown for a finding with no recorded file location.
pub(crate) const FINDING_NO_FILE: &str = "(no file)";
/// Placeholder shown for a finding not linked to any plan task.
pub(crate) const FINDING_UNASSIGNED: &str = "(unassigned)";

/// Collapse all internal whitespace (including newlines) to single spaces, so a
/// multiline issue/fix can't break the line-oriented handoff/markdown blocks.
pub(super) fn flatten_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn append_block(body: &mut String, text: &str) {
    let text = text.trim();
    if body.trim().is_empty() {
        *body = text.to_string();
    } else {
        let trimmed = body.trim_end();
        *body = format!("{trimmed}\n\n{text}");
    }
}

fn prepend_block(body: &mut String, text: &str) {
    let text = text.trim();
    if body.trim().is_empty() {
        *body = text.to_string();
    } else {
        let trimmed = body.trim_start();
        *body = format!("{text}\n\n{trimmed}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_finding(severity: Severity, issue: &str) -> Finding {
        Finding {
            severity,
            file: Some("src/foo.rs".to_string()),
            line: Some(42),
            issue: issue.to_string(),
            required_fix: "do the thing".to_string(),
            acceptance_tests: vec!["the test passes".to_string()],
            source_ids: vec!["call-1".to_string()],
            task: None,
            resolved: false,
        }
    }

    #[test]
    fn add_finding_dedupes_and_bumps_revision() {
        let mut doc = PlanDoc::default();
        let before = doc.revision;
        doc.add_finding(sample_finding(Severity::Major, "off-by-one"));
        doc.add_finding(sample_finding(Severity::Major, "off-by-one"));
        assert_eq!(doc.findings.len(), 1);
        assert!(doc.revision > before);
    }

    #[test]
    fn add_finding_refines_existing_in_place() {
        let mut doc = PlanDoc::default();
        let mut finding = sample_finding(Severity::Major, "same issue");
        finding.required_fix = "first fix".to_string();
        doc.add_finding(finding.clone());
        // Re-record the same identity with an improved fix + acceptance.
        finding.required_fix = "better fix".to_string();
        finding.acceptance_tests = vec!["the new check".to_string()];
        doc.add_finding(finding);
        assert_eq!(
            doc.findings.len(),
            1,
            "same identity refines, never appends"
        );
        assert_eq!(doc.findings[0].required_fix, "better fix");
        assert_eq!(
            doc.findings[0].acceptance_tests,
            vec!["the new check".to_string()],
            "the refined data must not be silently dropped"
        );
    }

    #[test]
    fn findings_handoff_block_flattens_multiline_text() {
        let mut doc = PlanDoc::default();
        let mut finding = sample_finding(Severity::Blocker, "line one\nline two");
        finding.required_fix = "do this\nthen that".to_string();
        doc.add_finding(finding);
        let block = doc.findings_handoff_block();
        assert!(
            !block.lines().any(|l| l == "line two" || l == "then that"),
            "multiline fields must be flattened so finding boundaries stay clear:\n{block}"
        );
        assert!(block.contains("line one line two"));
        assert!(block.contains("Required fix: do this then that"));
    }

    #[test]
    fn is_empty_and_clear_include_findings() {
        let mut doc = PlanDoc::default();
        doc.add_finding(sample_finding(Severity::Nit, "naming"));
        assert!(!doc.is_empty(), "a plan with only findings is not empty");
        doc.clear();
        assert!(doc.findings.is_empty());
        assert!(doc.is_empty());
    }

    #[test]
    fn add_finding_checked_rejects_empty_issue_or_fix() {
        let mut doc = PlanDoc::default();
        let mut f = sample_finding(Severity::Minor, "");
        assert!(doc.add_finding_checked(f.clone()).is_err());
        f.issue = "real issue".to_string();
        f.required_fix = "   ".to_string();
        assert!(doc.add_finding_checked(f).is_err());
        assert!(doc.findings.is_empty());
    }

    #[test]
    fn associate_finding_links_by_task_text() {
        let mut doc = PlanDoc::default();
        doc.add_task("Fix the parser");
        doc.add_finding(sample_finding(Severity::Blocker, "parser panics"));
        doc.associate_finding("panics", "parser").unwrap();
        assert_eq!(doc.findings[0].task.as_deref(), Some("Fix the parser"));
        assert!(doc.associate_finding("nope", "parser").is_err());
    }

    #[test]
    fn resolve_finding_marks_resolved_and_keeps_record() {
        let mut doc = PlanDoc::default();
        doc.add_finding(sample_finding(Severity::Major, "leak"));
        doc.resolve_finding("leak").unwrap();
        assert_eq!(doc.findings.len(), 1, "resolving never deletes");
        assert!(doc.findings[0].resolved);
    }

    #[test]
    fn resolve_finding_accepts_unique_ordered_token_match() {
        let mut doc = PlanDoc::default();
        doc.add_finding(sample_finding(
            Severity::Minor,
            "The new resource subsystem is compiled into the production crate but is not used",
        ));
        doc.resolve_finding("resource subsystem is compiled into production")
            .unwrap();
        assert!(doc.findings[0].resolved);
    }

    #[test]
    fn resolve_finding_rejects_ambiguous_ordered_token_match() {
        let mut doc = PlanDoc::default();
        doc.add_finding(sample_finding(
            Severity::Minor,
            "resource subsystem is compiled into the production crate",
        ));
        doc.add_finding(sample_finding(
            Severity::Major,
            "resource subsystem is compiled into the production helper",
        ));

        let error = doc
            .resolve_finding("resource subsystem compiled production")
            .unwrap_err();
        assert!(
            error.to_string().contains("Multiple findings match"),
            "ambiguous fuzzy matches must not silently pick a finding: {error}"
        );
        assert!(doc.findings.iter().all(|finding| !finding.resolved));
    }

    #[test]
    fn findings_handoff_block_orders_by_severity_and_marks_unassigned() {
        let mut doc = PlanDoc::default();
        doc.add_finding(sample_finding(Severity::Nit, "trailing whitespace"));
        doc.add_finding(sample_finding(Severity::Blocker, "data loss"));
        let block = doc.findings_handoff_block();
        let blocker = block.find("[BLOCKER]").unwrap();
        let nit = block.find("[NIT]").unwrap();
        assert!(blocker < nit, "most-severe first:\n{block}");
        assert!(block.contains("src/foo.rs:42 — data loss"));
        assert!(block.contains("Task: (unassigned)"));
        assert!(block.contains("Required fix: do the thing"));
        assert!(block.contains("Evidence: call-1"));
    }

    #[test]
    fn findings_handoff_block_skips_resolved_and_is_empty_when_none() {
        let mut doc = PlanDoc::default();
        assert!(doc.findings_handoff_block().is_empty());
        doc.add_finding(sample_finding(Severity::Major, "the bug"));
        doc.resolve_finding("the bug").unwrap();
        assert!(
            doc.findings_handoff_block().is_empty(),
            "resolved-only plans emit no handoff block"
        );
    }

    #[test]
    fn findings_markdown_is_not_reparsed_as_tasks() {
        let mut doc = PlanDoc::default();
        // A taskish section whose bullet IS parsed into a todo via the markdown
        // fallback (no structured tasks), alongside a finding. The finding's
        // `- **[BLOCKER]**` bullet must NOT also become a todo.
        doc.set_section("Tasks", "- do the real work");
        doc.add_finding(sample_finding(Severity::Blocker, "boom"));
        assert!(doc.to_markdown().contains("## Findings"));

        let todos = doc.tasks_as_todo_items();
        assert_eq!(
            todos.len(),
            1,
            "only the section bullet is a task, not the finding: {todos:?}"
        );
        assert_eq!(todos[0].content, "do the real work");
        assert!(
            !todos
                .iter()
                .any(|t| t.content.contains("boom") || t.content.contains("BLOCKER")),
            "a finding must never be parsed into a task: {todos:?}"
        );
    }

    #[test]
    fn set_section_replaces_by_heading_case_insensitively() {
        let mut doc = PlanDoc::default();
        doc.set_section("Approach", "first draft");
        doc.set_section("approach", "revised");

        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].body, "revised");
    }

    #[test]
    fn tasks_dedupe_and_check_by_substring() {
        let mut doc = PlanDoc::default();
        doc.add_task("Wire the canvas widget");
        doc.add_task("Wire the canvas widget");
        doc.add_task("Ship it");

        assert_eq!(doc.tasks.len(), 2);
        let checked = doc.check_task("canvas");
        assert_eq!(checked.as_deref(), Some("Wire the canvas widget"));
        assert!(doc.tasks[0].done);
        assert!(!doc.tasks[1].done);
    }

    #[test]
    fn patch_section_appends_prepends_replaces_text() {
        let mut doc = PlanDoc::default();
        doc.patch_section("Approach", SectionPatch::Replace("middle".to_string()))
            .unwrap();
        doc.patch_section("Approach", SectionPatch::Append("last".to_string()))
            .unwrap();
        doc.patch_section("Approach", SectionPatch::Prepend("first".to_string()))
            .unwrap();
        doc.patch_section(
            "Approach",
            SectionPatch::ReplaceText {
                old: "middle".to_string(),
                new: "second".to_string(),
            },
        )
        .unwrap();

        assert_eq!(doc.sections[0].body, "first\n\nsecond\n\nlast");
    }

    #[test]
    fn patch_section_rejects_missing_old_text() {
        let mut doc = PlanDoc::default();
        doc.set_section("Risks", "one");

        let err = doc
            .patch_section(
                "Risks",
                SectionPatch::ReplaceText {
                    old: "two".to_string(),
                    new: "three".to_string(),
                },
            )
            .unwrap_err();

        assert!(err.to_string().contains("does not contain"));
    }

    #[test]
    fn task_mutations_insert_update_remove_and_uncheck() {
        let mut doc = PlanDoc::default();
        doc.insert_task("B", InsertPosition::End, None).unwrap();
        doc.insert_task("A", InsertPosition::Before, Some("B"))
            .unwrap();
        doc.insert_task("C", InsertPosition::After, Some("B"))
            .unwrap();
        doc.update_task("B", "B revised").unwrap();
        doc.check_task("revised");
        let unchecked = doc.uncheck_task("revised");
        let removed = doc.remove_task("C").unwrap();

        assert_eq!(doc.tasks[0].text, "A");
        assert_eq!(doc.tasks[1].text, "B revised");
        assert!(!doc.tasks[1].done);
        assert_eq!(unchecked.as_deref(), Some("B revised"));
        assert!(removed.contains("C"));
        assert_eq!(doc.tasks.len(), 2);
    }

    #[test]
    fn insert_task_requires_target_for_relative_positions() {
        let mut doc = PlanDoc::default();
        let err = doc
            .insert_task("A", InsertPosition::Before, None)
            .unwrap_err();

        assert!(err.to_string().contains("target is required"));
    }

    fn headings(doc: &PlanDoc) -> Vec<&str> {
        doc.sections
            .iter()
            .map(|section| section.heading.as_str())
            .collect()
    }

    #[test]
    fn move_section_reorders_by_position() {
        let mut doc = PlanDoc::default();
        doc.set_section("Goal", "g");
        doc.set_section("Approach", "a");
        doc.set_section("Risks", "r");

        doc.move_section("Risks", InsertPosition::Start, None)
            .unwrap();
        assert_eq!(headings(&doc), ["Risks", "Goal", "Approach"]);

        doc.move_section("Risks", InsertPosition::End, None)
            .unwrap();
        assert_eq!(headings(&doc), ["Goal", "Approach", "Risks"]);

        doc.move_section("Risks", InsertPosition::Before, Some("Approach"))
            .unwrap();
        assert_eq!(headings(&doc), ["Goal", "Risks", "Approach"]);

        doc.move_section("Goal", InsertPosition::After, Some("Risks"))
            .unwrap();
        assert_eq!(headings(&doc), ["Risks", "Goal", "Approach"]);
    }

    #[test]
    fn move_section_missing_target_leaves_sections_unchanged() {
        let mut doc = PlanDoc::default();
        doc.set_section("Goal", "g");
        doc.set_section("Approach", "a");

        let err = doc
            .move_section("Goal", InsertPosition::Before, Some("Ghost"))
            .unwrap_err();
        assert!(err.to_string().contains("No plan section"));
        assert_eq!(headings(&doc), ["Goal", "Approach"]);
    }

    #[test]
    fn move_section_unknown_heading_errors() {
        let mut doc = PlanDoc::default();
        doc.set_section("Goal", "g");
        let err = doc
            .move_section("Ghost", InsertPosition::Start, None)
            .unwrap_err();
        assert!(err.to_string().contains("No plan section named 'Ghost'"));
    }

    #[test]
    fn questions_add_dedupe_remove_and_affect_emptiness() {
        let mut doc = PlanDoc::default();
        assert!(doc.is_empty());
        doc.add_question("Which auth method?");
        doc.add_question("Which auth method?");
        doc.add_question("Rate limit per provider?");
        assert_eq!(doc.questions.len(), 2);
        assert!(!doc.is_empty());

        let removed = doc.remove_question("auth").unwrap();
        assert!(removed.contains("Which auth method?"));
        assert_eq!(doc.questions, ["Rate limit per provider?"]);

        assert!(doc.remove_question("ghost").is_err());
    }

    #[test]
    fn markdown_includes_open_questions_between_sections_and_tasks() {
        let mut doc = PlanDoc::default();
        doc.set_section("Goal", "Ship it.");
        doc.add_question("Which auth method?");
        doc.add_task("Wire it up");

        let md = doc.to_markdown();
        let questions = md.find("## Open questions").expect("questions block");
        let tasks = md.find("## Tasks").expect("tasks block");
        assert!(md.contains("- Which auth method?"));
        assert!(questions < tasks, "questions should precede tasks: {md}");
    }

    #[test]
    fn markdown_includes_title_sections_and_tasks() {
        let mut doc = PlanDoc::default();
        doc.set_title("Feature X");
        doc.set_section("Goal", "Do the thing.");
        doc.add_task("Step one");
        doc.check_task("Step one");

        let md = doc.to_markdown();
        assert!(md.starts_with("# Feature X"));
        assert!(md.contains("## Goal"));
        assert!(md.contains("- [x] Step one"));
    }

    #[test]
    fn handoff_markdown_preserves_implementation_details() {
        let mut doc = PlanDoc::default();
        doc.set_title("Handoff plan");
        doc.set_section(
            "Implementation details",
            "Change `src/example.rs::run` and preserve its caller contract.",
        );
        doc.add_task("Implement the change");

        let markdown = doc.to_markdown_for_handoff();

        assert!(markdown.contains("## Implementation details"));
        assert!(markdown.contains("`src/example.rs::run`"));
        assert!(markdown.contains("preserve its caller contract"));
    }

    #[test]
    fn legacy_saved_plan_deserializes_and_hands_off_without_implementation_details() {
        let snapshot = serde_json::json!({
            "title": "Legacy saved plan",
            "sections": [{
                "heading": "Approach",
                "body": "Preserve snapshots created before the handoff contract."
            }],
            "questions": [],
            "tasks": [{"text": "Implement the legacy plan", "done": false}],
            "revision": 1
        });
        let doc: PlanDoc = serde_json::from_value(snapshot)
            .expect("legacy saved plan snapshot should remain compatible");

        let markdown = doc.to_markdown_for_handoff();

        assert!(markdown.contains("## Approach"));
        assert!(markdown.contains("- [ ] Implement the legacy plan"));
    }

    #[test]
    fn markdown_body_omits_the_title_heading() {
        let mut doc = PlanDoc::default();
        doc.set_title("Feature X");
        doc.set_section("Goal", "Do the thing.");
        doc.add_task("Step one");

        // The canvas body is rendered alongside the title (which now lives in
        // the pane's frame header), so the body markdown must skip the
        // leading "# Title" to avoid duplicating it on screen.
        let body = doc.to_markdown_body();
        assert!(
            !body.starts_with("# Feature X"),
            "body markdown should not repeat the title, got: {body:?}"
        );
        assert!(body.contains("## Goal"));
        assert!(body.contains("- [ ] Step one"));
    }

    #[test]
    fn empty_doc_reports_empty_and_revision_tracks_changes() {
        let mut doc = PlanDoc::default();
        assert!(doc.is_empty());
        doc.set_title("t");
        assert!(!doc.is_empty());
        let rev = doc.revision;
        doc.clear();
        assert!(doc.is_empty());
        assert!(doc.revision > rev);
    }

    #[test]
    fn tasks_as_todo_items_marks_done_as_completed() {
        use crate::todo::TodoStatus;
        let mut doc = PlanDoc::default();
        doc.add_task("Already done");
        doc.check_task("Already done");
        doc.add_task("Next up");
        let items = doc.tasks_as_todo_items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn tasks_as_todo_items_promotes_only_first_pending() {
        use crate::todo::TodoStatus;
        let mut doc = PlanDoc::default();
        doc.add_task("A");
        doc.add_task("B");
        doc.add_task("C");
        let items = doc.tasks_as_todo_items();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].status, TodoStatus::InProgress);
        assert_eq!(items[1].status, TodoStatus::Pending);
        assert_eq!(items[2].status, TodoStatus::Pending);
    }

    #[test]
    fn tasks_as_todo_items_preserves_order() {
        let mut doc = PlanDoc::default();
        doc.add_task("first");
        doc.add_task("second");
        doc.add_task("third");
        let items = doc.tasks_as_todo_items();
        assert_eq!(items[0].content, "first");
        assert_eq!(items[1].content, "second");
        assert_eq!(items[2].content, "third");
    }

    #[test]
    fn tasks_as_todo_items_empty_list_returns_empty() {
        let doc = PlanDoc::default();
        let items = doc.tasks_as_todo_items();
        assert!(items.is_empty());
    }

    #[test]
    fn tasks_as_todo_items_extracts_markdown_checklists_when_structured_tasks_are_empty() {
        use crate::todo::TodoStatus;
        let mut doc = PlanDoc::default();
        doc.set_section(
            "Implementation",
            "- [x] Preserve existing sessions\n- [ ] Wire /start\n- [ ] Run tests",
        );

        let items = doc.tasks_as_todo_items();

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].content, "Preserve existing sessions");
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].content, "Wire /start");
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert_eq!(items[2].content, "Run tests");
        assert_eq!(items[2].status, TodoStatus::Pending);
    }

    #[test]
    fn tasks_as_todo_items_extracts_taskish_section_bullets() {
        let mut doc = PlanDoc::default();
        doc.set_section("Implementation tasks", "1. Read code\n2. Patch handoff");

        let items = doc.tasks_as_todo_items();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content, "Read code");
        assert_eq!(items[1].content, "Patch handoff");
    }

    #[test]
    fn tasks_as_todo_items_all_done_returns_all_completed() {
        use crate::todo::TodoStatus;
        let mut doc = PlanDoc::default();
        doc.add_task("A");
        doc.add_task("B");
        doc.check_task("A");
        doc.check_task("B");
        let items = doc.tasks_as_todo_items();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|t| t.status == TodoStatus::Completed));
    }

    fn phase_names(doc: &PlanDoc) -> Vec<&str> {
        doc.phases.iter().map(|phase| phase.name.as_str()).collect()
    }

    #[test]
    fn add_phase_dedupes_and_tracks_phased_state() {
        let mut doc = PlanDoc::default();
        assert!(!doc.is_phased());
        doc.add_phase("Phase 1");
        doc.add_phase("phase 1"); // case-insensitive dedupe
        doc.add_phase("Phase 2");
        assert_eq!(phase_names(&doc), ["Phase 1", "Phase 2"]);
        assert!(doc.is_phased());
        assert!(!doc.is_empty());
    }

    #[test]
    fn add_phase_checked_rejects_blank_and_duplicate() {
        let mut doc = PlanDoc::default();
        assert!(doc.add_phase_with_tasks_checked("  ", &[]).is_err());
        doc.add_phase_with_tasks_checked("Phase 1", &[]).unwrap();
        assert!(doc.add_phase_with_tasks_checked("phase 1", &[]).is_err());
    }

    #[test]
    fn add_phase_with_tasks_creates_initial_checklist() {
        let mut doc = PlanDoc::default();
        let tasks = vec![
            "create migration".to_string(),
            "wire command".to_string(),
            "create migration".to_string(),
        ];

        let count = doc.add_phase_with_tasks_checked("Phase 1", &tasks).unwrap();

        assert_eq!(count, 2);
        assert_eq!(phase_names(&doc), ["Phase 1"]);
        let texts: Vec<&str> = doc.phases[0]
            .tasks
            .iter()
            .map(|task| task.text.as_str())
            .collect();
        assert_eq!(texts, ["create migration", "wire command"]);
        assert!(doc.phases[0].tasks.iter().all(|task| !task.done));
    }

    #[test]
    fn add_phase_with_tasks_rejects_blank_task_without_creating_phase() {
        let mut doc = PlanDoc::default();
        let tasks = vec!["create migration".to_string(), "  ".to_string()];

        let err = doc
            .add_phase_with_tasks_checked("Phase 1", &tasks)
            .unwrap_err();

        assert!(err.to_string().contains("phase task text"));
        assert!(doc.phases.is_empty());
    }

    #[test]
    fn phase_task_add_insert_and_revision_bump() {
        let mut doc = PlanDoc::default();
        doc.add_phase("Build");
        let rev = doc.revision;
        doc.add_task_to_phase("Build", "compile").unwrap();
        doc.insert_task_to_phase("Build", "lint", InsertPosition::Start, None)
            .unwrap();
        doc.insert_task_to_phase("Build", "test", InsertPosition::After, Some("compile"))
            .unwrap();
        let texts: Vec<&str> = doc.phases[0]
            .tasks
            .iter()
            .map(|t| t.text.as_str())
            .collect();
        assert_eq!(texts, ["lint", "compile", "test"]);
        assert!(doc.revision > rev);
    }

    #[test]
    fn add_task_to_unknown_phase_errors() {
        let mut doc = PlanDoc::default();
        let err = doc.add_task_to_phase("Ghost", "x").unwrap_err();
        assert!(err.to_string().contains("No plan phase named 'Ghost'"));
    }

    #[test]
    fn move_phase_reorders_and_restores_on_bad_target() {
        let mut doc = PlanDoc::default();
        doc.add_phase("A");
        doc.add_phase("B");
        doc.add_phase("C");
        doc.move_phase("C", InsertPosition::Start, None).unwrap();
        assert_eq!(phase_names(&doc), ["C", "A", "B"]);
        let err = doc
            .move_phase("A", InsertPosition::Before, Some("Ghost"))
            .unwrap_err();
        assert!(err.to_string().contains("No plan phase"));
        assert_eq!(phase_names(&doc), ["C", "A", "B"]);
    }

    #[test]
    fn matching_ops_reach_into_phase_tasks() {
        let mut doc = PlanDoc::default();
        doc.add_phase("Phase 1");
        doc.add_task_to_phase("Phase 1", "wire the handoff")
            .unwrap();
        // check / update / remove all locate tasks living inside phases.
        assert_eq!(
            doc.check_task("handoff").as_deref(),
            Some("wire the handoff")
        );
        assert!(doc.phases[0].tasks[0].done);
        doc.uncheck_task("handoff");
        assert!(!doc.phases[0].tasks[0].done);
        doc.update_task("handoff", "wire the phase handoff")
            .unwrap();
        assert_eq!(doc.phases[0].tasks[0].text, "wire the phase handoff");
        doc.remove_task("phase handoff").unwrap();
        assert!(doc.phases[0].tasks.is_empty());
    }

    #[test]
    fn phase_todo_items_scopes_to_one_phase_and_promotes_first_open() {
        use crate::todo::TodoStatus;
        let mut doc = PlanDoc::default();
        doc.add_phase("One");
        doc.add_phase("Two");
        doc.add_task_to_phase("One", "a").unwrap();
        doc.add_task_to_phase("One", "b").unwrap();
        doc.add_task_to_phase("Two", "c").unwrap();
        doc.check_task("a");

        let one = doc.phase_todo_items(0);
        assert_eq!(one.len(), 2);
        assert_eq!(one[0].status, TodoStatus::Completed);
        assert_eq!(one[1].status, TodoStatus::InProgress);

        let two = doc.phase_todo_items(1);
        assert_eq!(two.len(), 1);
        assert_eq!(two[0].content, "c");
        assert_eq!(two[0].status, TodoStatus::InProgress);

        assert!(doc.phase_todo_items(9).is_empty());
    }

    #[test]
    fn next_phase_with_pending_and_mark_done_drive_advance() {
        let mut doc = PlanDoc::default();
        doc.add_phase("One");
        doc.add_phase("Two");
        doc.add_phase("Three");
        doc.add_task_to_phase("One", "a").unwrap();
        doc.add_task_to_phase("Two", "b").unwrap();
        doc.add_task_to_phase("Three", "c").unwrap();

        assert_eq!(doc.next_phase_with_pending(None), Some(0));
        doc.mark_phase_done(0);
        assert_eq!(doc.next_phase_with_pending(Some(0)), Some(1));
        doc.mark_phase_done(1);
        doc.mark_phase_done(2);
        assert_eq!(doc.next_phase_with_pending(Some(2)), None);
        // All tasks marked done by mark_phase_done.
        assert!(doc.phases.iter().all(|p| p.tasks.iter().all(|t| t.done)));
    }

    #[test]
    fn markdown_renders_phase_headings_and_checklists() {
        let mut doc = PlanDoc::default();
        doc.set_title("Feature");
        doc.set_section("Context", "why");
        doc.add_phase("Phase 1: storage");
        doc.add_task_to_phase("Phase 1: storage", "add table")
            .unwrap();
        doc.check_task("add table");
        doc.add_phase("Phase 2: wiring");
        doc.add_task_to_phase("Phase 2: wiring", "wire it").unwrap();

        let md = doc.to_markdown();
        assert!(md.contains("## Phase 1: storage"));
        assert!(md.contains("- [x] add table"));
        assert!(md.contains("## Phase 2: wiring"));
        assert!(md.contains("- [ ] wire it"));
        // Context section precedes the phases.
        assert!(md.find("## Context").unwrap() < md.find("## Phase 1").unwrap());
    }
}
