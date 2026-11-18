//! Plan-canvas tools, available only to the planning agent. A complete first
//! draft is written atomically; granular tools handle later corrections.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::episode::EpisodeAction;
use crate::plan::{Finding, InsertPosition, PlanDoc, SectionPatch, Severity, SharedPlanStore};
use crate::storage::Storage;
use crate::tool::schema::{
    array_property, closed_object, integer_property, parse_args, string_enum_property,
    string_property,
};
use crate::tool::{SharedActiveSessionId, Tool, ToolOutput};

macro_rules! plan_tool {
    (
        $tool:ident,
        $args:ty,
        name: $name:literal,
        description: $description:expr,
        schema: $schema:expr,
        execute: |$store:ident, $parsed:ident| $body:block
    ) => {
        pub struct $tool {
            store: SharedPlanStore,
        }

        impl $tool {
            pub fn new(store: SharedPlanStore) -> Self {
                Self { store }
            }
        }

        #[async_trait]
        impl Tool for $tool {
            fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
                crate::tool::ToolEffectPolicy::LocalState
            }

            fn name(&self) -> &str {
                $name
            }

            fn description(&self) -> &str {
                $description
            }

            fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
                crate::tool::ParallelPolicy::Serialized
            }

            fn parameters_schema(&self) -> serde_json::Value {
                $schema
            }

            async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
                let $parsed: $args = parse_args($name, args)?;
                let $store = &self.store;
                $body
            }
        }
    };
}

#[cfg(test)]
#[derive(Deserialize)]
struct SetTitleArgs {
    title: String,
}

#[cfg(test)]
#[derive(Deserialize)]
struct SetSectionArgs {
    heading: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PatchSectionOperationArg {
    Replace,
    Append,
    Prepend,
    ReplaceText,
    Remove,
    Move,
}

#[derive(Deserialize)]
struct PatchSectionArgs {
    heading: String,
    operation: PatchSectionOperationArg,
    text: Option<String>,
    old_text: Option<String>,
    new_text: Option<String>,
    position: Option<InsertPositionArg>,
    target: Option<String>,
}

/// A validated `plan_patch_section` request.
enum SectionEdit {
    Patch(SectionPatch),
    Remove,
    Move {
        position: InsertPosition,
        target: Option<String>,
    },
}

impl PatchSectionArgs {
    /// Normalize the flat wire fields into a typed [`SectionEdit`], validating
    /// that the operands required by the chosen operation are present. This is
    /// the single boundary where illegal field combinations are rejected, so
    /// everything downstream works with a value that cannot be malformed.
    fn into_heading_and_edit(self) -> Result<(String, SectionEdit)> {
        let PatchSectionArgs {
            heading,
            operation,
            text,
            old_text,
            new_text,
            position,
            target,
        } = self;
        let edit = match operation {
            PatchSectionOperationArg::Replace => {
                SectionEdit::Patch(SectionPatch::Replace(require_operand(text, "text")?))
            }
            PatchSectionOperationArg::Append => {
                SectionEdit::Patch(SectionPatch::Append(require_operand(text, "text")?))
            }
            PatchSectionOperationArg::Prepend => {
                SectionEdit::Patch(SectionPatch::Prepend(require_operand(text, "text")?))
            }
            PatchSectionOperationArg::ReplaceText => {
                SectionEdit::Patch(SectionPatch::ReplaceText {
                    old: require_operand(old_text, "old_text")?,
                    new: new_text.unwrap_or_default(),
                })
            }
            PatchSectionOperationArg::Remove => SectionEdit::Remove,
            PatchSectionOperationArg::Move => SectionEdit::Move {
                position: position
                    .ok_or_else(|| anyhow::anyhow!("move requires position"))?
                    .into(),
                target,
            },
        };
        Ok((heading, edit))
    }
}

/// Trims a flat operand and rejects it when blank, mirroring the per-operation
/// required-field rules the patch wire schema cannot express on its own.
fn require_operand(value: Option<String>, field: &str) -> Result<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{field} must not be empty"))
}

#[cfg(test)]
#[derive(Deserialize)]
struct AddTaskArgs {
    text: String,
    /// When set, the task is appended to this phase's checklist instead of the
    /// flat top-level list. The phase must already exist.
    #[serde(default)]
    phase: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum InsertPositionArg {
    Start,
    End,
    Before,
    After,
}

impl From<InsertPositionArg> for InsertPosition {
    fn from(value: InsertPositionArg) -> Self {
        match value {
            InsertPositionArg::Start => Self::Start,
            InsertPositionArg::End => Self::End,
            InsertPositionArg::Before => Self::Before,
            InsertPositionArg::After => Self::After,
        }
    }
}

#[cfg(test)]
#[derive(Deserialize)]
struct AddPhaseArgs {
    name: String,
    #[serde(default)]
    tasks: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct DraftSectionArgs {
    heading: String,
    body: String,
}

#[derive(Deserialize)]
struct DraftPhaseArgs {
    name: String,
    tasks: Vec<String>,
}

#[derive(Deserialize)]
struct ReplaceDraftArgs {
    title: String,
    #[serde(default, rename = "episode_action")]
    _episode_action: EpisodeAction,
    #[serde(default)]
    sections: Vec<DraftSectionArgs>,
    #[serde(default)]
    tasks: Vec<String>,
    #[serde(default)]
    phases: Vec<DraftPhaseArgs>,
    #[serde(default)]
    questions: Vec<String>,
}

const IMPLEMENTATION_DETAILS_HEADING: &str = "Implementation details";

impl ReplaceDraftArgs {
    fn into_plan(self) -> Result<PlanDoc> {
        let ReplaceDraftArgs {
            title,
            _episode_action: _,
            sections,
            tasks,
            phases,
            questions,
        } = self;
        if title.trim().chars().count() > 80 {
            anyhow::bail!("title must be 80 characters or fewer");
        }
        if !tasks.is_empty() && !phases.is_empty() {
            anyhow::bail!("a plan must use either flat tasks or phases, not both");
        }
        if tasks.is_empty() && phases.is_empty() {
            anyhow::bail!("a plan must include at least one task or phase");
        }
        if !questions.is_empty() {
            anyhow::bail!(
                "plans cannot contain open questions; ask the user with the question tool first"
            );
        }
        let mut plan = PlanDoc::default();
        {
            let mut editor = plan.edit();
            editor.set_title_checked(&title)?;
            for section in sections {
                editor.set_section_checked(&section.heading, &section.body)?;
            }
            for task in tasks {
                editor.add_task_checked(&task)?;
            }
            for phase in phases {
                editor.add_phase_with_tasks_checked(&phase.name, &phase.tasks)?;
            }
        }
        let has_implementation_details = plan.sections.iter().any(|section| {
            section
                .heading
                .eq_ignore_ascii_case(IMPLEMENTATION_DETAILS_HEADING)
                && !section.body.trim().is_empty()
        });
        if !has_implementation_details {
            anyhow::bail!(
                "a plan must include a non-empty '{IMPLEMENTATION_DETAILS_HEADING}' section"
            );
        }
        Ok(plan)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskOperationArg {
    Insert,
    Update,
    Remove,
    Check,
    Uncheck,
    RemovePhase,
    MovePhase,
}

#[derive(Deserialize)]
struct TasksArgs {
    operation: TaskOperationArg,
    text: Option<String>,
    target: Option<String>,
    position: Option<InsertPositionArg>,
    phase: Option<String>,
}

/// Trims an optional selector and drops it when blank.
fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

impl TasksArgs {
    /// Apply one task or phase operation, validating its required fields.
    fn apply(self, doc: &mut PlanDoc) -> Result<String> {
        let TasksArgs {
            operation,
            text,
            target,
            position,
            phase,
        } = self;
        let phase = non_blank(phase);
        match operation {
            TaskOperationArg::Insert => {
                let text = require_operand(text, "text")?;
                let position = position.map_or(InsertPosition::End, Into::into);
                match phase {
                    Some(phase) => {
                        doc.edit()
                            .insert_task_to_phase(&phase, &text, position, target.as_deref())
                    }
                    None => doc.edit().insert_task(&text, position, target.as_deref()),
                }
            }
            TaskOperationArg::Update => {
                let target = require_operand(target, "target")?;
                let text = require_operand(text, "text")?;
                doc.edit().update_task(&target, &text)
            }
            TaskOperationArg::Remove => {
                let target = require_operand(target, "target")?;
                doc.edit().remove_task(&target)
            }
            TaskOperationArg::Check => {
                let target = require_operand(target, "target")?;
                match doc.edit().check_task(&target) {
                    Some(text) => Ok(format!("Checked off: {text}")),
                    None => anyhow::bail!("No unchecked task matching '{target}' found."),
                }
            }
            TaskOperationArg::Uncheck => {
                let target = require_operand(target, "target")?;
                match doc.edit().uncheck_task(&target) {
                    Some(text) => Ok(format!("Unchecked: {text}")),
                    None => anyhow::bail!("No checked task matching '{target}' found."),
                }
            }
            TaskOperationArg::RemovePhase => {
                let phase = phase.ok_or_else(|| anyhow::anyhow!("remove_phase requires phase"))?;
                if doc.edit().remove_phase(&phase) {
                    Ok(format!("Plan phase '{phase}' removed."))
                } else {
                    anyhow::bail!("No plan phase named '{phase}' found.")
                }
            }
            TaskOperationArg::MovePhase => {
                let phase = phase.ok_or_else(|| anyhow::anyhow!("move_phase requires phase"))?;
                let position =
                    position.ok_or_else(|| anyhow::anyhow!("move_phase requires position"))?;
                doc.edit()
                    .move_phase(&phase, position.into(), target.as_deref())
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum FindingOperationArg {
    Add,
    Associate,
    Resolve,
}

#[derive(Deserialize)]
struct FindingsArgs {
    operation: FindingOperationArg,
    severity: Option<String>,
    issue: Option<String>,
    required_fix: Option<String>,
    file: Option<String>,
    line: Option<u32>,
    acceptance_tests: Option<Vec<String>>,
    source_ids: Option<Vec<String>>,
    task: Option<String>,
    finding: Option<String>,
}

impl FindingsArgs {
    /// Apply one finding operation, validating its required fields.
    fn apply(self, doc: &mut PlanDoc) -> Result<String> {
        let FindingsArgs {
            operation,
            severity,
            issue,
            required_fix,
            file,
            line,
            acceptance_tests,
            source_ids,
            task,
            finding,
        } = self;
        match operation {
            FindingOperationArg::Add => {
                let severity_text = require_operand(severity, "severity")?;
                let severity = Severity::from_db_str(&severity_text).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid severity {severity_text:?}; expected one of blocker|major|minor|nit"
                    )
                })?;
                let finding = Finding {
                    severity,
                    file: non_blank(file),
                    line,
                    issue: require_operand(issue, "issue")?,
                    required_fix: require_operand(required_fix, "required_fix")?,
                    acceptance_tests: acceptance_tests.unwrap_or_default(),
                    source_ids: source_ids.unwrap_or_default(),
                    task: non_blank(task),
                    resolved: false,
                };
                doc.edit().add_finding_checked(finding)?;
                let open = doc.findings.iter().filter(|f| !f.resolved).count();
                Ok(format!("Finding recorded ({open} open)."))
            }
            FindingOperationArg::Associate => {
                let finding = require_operand(finding, "finding")?;
                let task = require_operand(task, "task")?;
                doc.edit().associate_finding(&finding, &task)
            }
            FindingOperationArg::Resolve => {
                let finding = require_operand(finding, "finding")?;
                doc.edit().resolve_finding(&finding)
            }
        }
    }
}

/// Update the plan canvas title and mirror it to the active session so the
/// TUI header and session pickers stay in sync with the plan the user is
/// reviewing. The session update is a no-op when no persisted session is
/// active (e.g., during unit tests or eval), since the plan itself is what
/// matters there.
#[cfg(test)]
pub struct PlanSetTitleTool {
    store: SharedPlanStore,
    storage: Storage,
    active_session_id: SharedActiveSessionId,
}

/// Atomically replace the editable plan draft while preserving review findings.
pub struct PlanReplaceDraftTool {
    store: SharedPlanStore,
    storage: Storage,
    active_session_id: SharedActiveSessionId,
}

impl PlanReplaceDraftTool {
    pub fn new(
        store: SharedPlanStore,
        storage: Storage,
        active_session_id: SharedActiveSessionId,
    ) -> Self {
        Self {
            store,
            storage,
            active_session_id,
        }
    }
}

#[async_trait]
impl Tool for PlanReplaceDraftTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "plan_replace_draft"
    }

    fn description(&self) -> &str {
        "Write a complete plan draft atomically in one call: title, ordered sections including \
         a non-empty `Implementation details` handoff section, and \
         either flat tasks or phases. Use this for the initial canvas and \
         wholesale restructuring; it preserves structured review findings. Empty collection \
         fields may be omitted. Declare whether the plan starts a distinct user topic; \
         corrections and restructuring of the current request are same_topic. Use granular plan \
         tools only for later corrections."
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "title",
                    string_property("Short plan title, at most 80 characters"),
                ),
                (
                    "episode_action",
                    string_enum_property(
                        "new_topic for a distinct user goal; same_topic when refining the current request",
                        EpisodeAction::WIRE_VALUES,
                    ),
                ),
                (
                    "sections",
                    array_property(
                        "Ordered concise plan sections; must include a non-empty `Implementation details` section for the coding-agent handoff",
                        closed_object(
                            [
                                ("heading", string_property("Section heading")),
                                ("body", string_property("Concise markdown section body")),
                            ],
                            &["heading", "body"],
                        ),
                    ),
                ),
                (
                    "tasks",
                    array_property(
                        "Ordered flat implementation tasks; use [] when phases are present",
                        string_property(
                            "Short verb-first action label, anchored to its file or function when one exists",
                        ),
                    ),
                ),
                (
                    "phases",
                    array_property(
                        "Ordered execution phases; use [] for a flat plan",
                        closed_object(
                            [
                                ("name", string_property("Short phase name")),
                                (
                                    "tasks",
                                    array_property(
                                        "Ordered tasks owned by this phase",
                                        string_property(
                                            "Short verb-first action label, anchored to its file or function when one exists",
                                        ),
                                    ),
                                ),
                            ],
                            &["name", "tasks"],
                        ),
                    ),
                ),
            ],
            &["title", "episode_action", "sections"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let replacement: ReplaceDraftArgs = parse_args(self.name(), args)?;
        let mut replacement = replacement.into_plan()?;
        let section_count = replacement.sections.len();
        let phase_count = replacement.phases.len();
        let task_count = replacement.tasks.len()
            + replacement
                .phases
                .iter()
                .map(|phase| phase.tasks.len())
                .sum::<usize>();
        let question_count = replacement.questions.len();
        let title = replacement.title.clone();
        {
            let mut plan = self.store.lock().await;
            let findings = std::mem::take(&mut plan.findings);
            replacement.findings = findings;
            replacement.revision = plan.revision.saturating_add(1);
            *plan = replacement;
        }
        if let Some(session_id) = *self.active_session_id.lock().await {
            self.storage.set_session_summary(session_id, &title).await?;
        }
        Ok(ToolOutput::Text(format!(
            "Plan draft replaced: {section_count} sections, {task_count} tasks, {phase_count} phases, {question_count} questions."
        )))
    }
}

#[cfg(test)]
impl PlanSetTitleTool {
    pub fn new(
        store: SharedPlanStore,
        storage: Storage,
        active_session_id: SharedActiveSessionId,
    ) -> Self {
        Self {
            store,
            storage,
            active_session_id,
        }
    }
}

#[cfg(test)]
#[async_trait]
impl Tool for PlanSetTitleTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "plan_set_title"
    }

    fn description(&self) -> &str {
        "Set the title of the plan canvas the user is reviewing. The session \
         title is mirrored to match, so the conversation header and session \
         pickers stay in sync with the plan."
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object([("title", string_property("Short plan title"))], &["title"])
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: SetTitleArgs = parse_args("plan_set_title", args)?;
        // Both checks upfront: the empty-title invariant is centralized in
        // `set_title_checked`; the 80-char cap mirrors `set_session_title` so
        // the mirrored session summary can't carry more than the column we
        // share with that tool.
        let trimmed = args.title.trim();
        if trimmed.chars().count() > 80 {
            anyhow::bail!("title must be 80 characters or fewer");
        }
        let title = {
            let mut doc = self.store.lock().await;
            doc.edit().set_title_checked(trimmed)?;
            doc.title.clone()
        };
        // Mirror to the active session. No-op without one — the plan update
        // already succeeded and we don't want to fail the tool just because
        // there's no persisted session (tests, eval, headless replays).
        if let Some(session_id) = *self.active_session_id.lock().await {
            self.storage.set_session_summary(session_id, &title).await?;
        }
        Ok(ToolOutput::Text(format!("Plan title set to: {title}")))
    }
}

#[cfg(test)]
plan_tool!(
    PlanSetSectionTool,
    SetSectionArgs,
    name: "plan_set_section",
    description: "Create or replace a section of the plan. The body is markdown — keep it \
         brief: the intended change plus any file or decision the coding agent could not \
         infer. Sections keep their order; writing an existing heading replaces that \
         section's body.",
    schema: closed_object(
        [
            (
                "heading",
                string_property("Section heading, e.g. 'Approach'"),
            ),
            (
                "body",
                string_property(
                    "Concise markdown: the intended change and the one or two load-bearing files/functions or decisions. Omit narration, edge-case dumps, and validation commands unless non-obvious",
                ),
            ),
        ],
        &["heading", "body"],
    ),
    execute: |store, args| {
        let mut doc = store.lock().await;
        doc.edit()
            .set_section_checked(&args.heading, &args.body)?;
        Ok(ToolOutput::Text(format!(
            "Plan section '{}' updated ({} sections total).",
            args.heading.trim(),
            doc.sections.len()
        )))
    }
);

plan_tool!(
    PlanPatchSectionTool,
    PatchSectionArgs,
    name: "plan_patch_section",
    description: "Edit one plan section by heading (matched case-insensitively). Operations: \
         replace, append, prepend (require text); replace_text (requires old_text, accepts \
         new_text); remove; move (requires position: start|end|before|after, plus target \
         heading for before/after). Use plan_replace_draft for wholesale rewrites.",
    schema: closed_object(
        [
            ("heading", string_property("Section heading to edit")),
            (
                "operation",
                string_enum_property(
                    "Section operation to apply",
                    &["replace", "append", "prepend", "replace_text", "remove", "move"],
                ),
            ),
            (
                "text",
                string_property("Markdown context for replace, append, or prepend"),
            ),
            (
                "old_text",
                string_property("Existing text to replace for replace_text"),
            ),
            (
                "new_text",
                string_property("Replacement text for replace_text"),
            ),
            (
                "position",
                string_enum_property(
                    "Where to move the section (move only)",
                    &["start", "end", "before", "after"],
                ),
            ),
            (
                "target",
                string_property("Section heading to move before/after (move only)"),
            ),
        ],
        &["heading", "operation"],
    ),
    execute: |store, args| {
        let (heading, edit) = args.into_heading_and_edit()?;
        let mut doc = store.lock().await;
        let message = match edit {
            SectionEdit::Patch(patch) => doc.edit().patch_section(&heading, patch)?,
            SectionEdit::Remove => {
                if doc.edit().remove_section(&heading) {
                    format!("Plan section '{}' removed.", heading.trim())
                } else {
                    anyhow::bail!("No plan section named '{}' found.", heading.trim())
                }
            }
            SectionEdit::Move { position, target } => {
                doc.edit().move_section(&heading, position, target.as_deref())?
            }
        };
        Ok(ToolOutput::Text(message))
    }
);

plan_tool!(
    PlanTasksTool,
    TasksArgs,
    name: "plan_tasks",
    description: "Edit the plan's task checklist and phases after the initial draft. Keep task \
         text a short verb-first todo label; detailed context lives in sections. Operations: \
         insert (text; position start|end|before|after, default end; target task for \
         before/after; phase to insert into that phase's checklist); update (target, text; \
         keeps done state); remove, check, uncheck (target); remove_phase (phase, removes its \
         tasks); move_phase (phase, position, target phase for before/after). A target \
         matches the first task whose text contains it.",
    schema: closed_object(
        [
            (
                "operation",
                string_enum_property(
                    "Task or phase operation to apply",
                    &[
                        "insert",
                        "update",
                        "remove",
                        "check",
                        "uncheck",
                        "remove_phase",
                        "move_phase",
                    ],
                ),
            ),
            (
                "text",
                string_property(
                    "Task label for insert/update, anchored to its file or function when one exists",
                ),
            ),
            (
                "target",
                string_property(
                    "Existing task text to match (update/remove/check/uncheck, insert before/after) or phase name (move_phase before/after)",
                ),
            ),
            (
                "position",
                string_enum_property(
                    "Placement for insert and move_phase",
                    &["start", "end", "before", "after"],
                ),
            ),
            (
                "phase",
                string_property(
                    "Phase name: the checklist for insert, or the phase for remove_phase/move_phase",
                ),
            ),
        ],
        &["operation"],
    ),
    execute: |store, args| {
        let mut doc = store.lock().await;
        Ok(ToolOutput::Text(args.apply(&mut doc)?))
    }
);

plan_tool!(
    PlanFindingsTool,
    FindingsArgs,
    name: "plan_findings",
    description: "Record and track review findings as structured plan data, so they survive the \
         handoff to implementation and cannot be silently dropped; prefer this over only \
         describing an issue in prose. Operations: add (severity, issue, required_fix; \
         optional file, line, acceptance_tests, source_ids, task); associate (finding, task: \
         link a finding to the plan task that addresses it); resolve (finding: mark it \
         addressed; findings are kept, never deleted). `finding` matches the issue text or \
         unique ordered keywords.",
    schema: closed_object(
        [
            (
                "operation",
                string_enum_property("Finding operation to apply", &["add", "associate", "resolve"]),
            ),
            (
                "severity",
                string_enum_property(
                    "Finding severity, most-to-least urgent (add)",
                    &["blocker", "major", "minor", "nit"],
                ),
            ),
            (
                "issue",
                string_property("What is wrong — the problem and supporting evidence (add)"),
            ),
            (
                "required_fix",
                string_property("The concrete change required to address the finding (add)"),
            ),
            ("file", string_property("Path of the file the finding refers to (add)")),
            ("line", integer_property("Line number within `file` (add)")),
            (
                "acceptance_tests",
                array_property(
                    "How to confirm the finding is fixed (add)",
                    string_property("An acceptance check"),
                ),
            ),
            (
                "source_ids",
                array_property(
                    "Source tool-call or transcript ids that evidence this finding (add)",
                    string_property("A source id"),
                ),
            ),
            (
                "task",
                string_property("Text of the plan task that addresses the finding (add, associate)"),
            ),
            (
                "finding",
                string_property(
                    "Text or ordered keywords matching an existing finding's issue (associate, resolve)",
                ),
            ),
        ],
        &["operation"],
    ),
    execute: |store, args| {
        let mut doc = store.lock().await;
        Ok(ToolOutput::Text(args.apply(&mut doc)?))
    }
);

#[cfg(test)]
plan_tool!(
    PlanAddTaskTool,
    AddTaskArgs,
    name: "plan_add_task",
    description: "Append a short implementation todo label to the plan's task checklist. \
         Put detailed context in plan sections, not in the task text. Pass `phase` to add \
         the task to a specific phase (created with plan_add_phase) instead of the flat list.",
    schema: closed_object(
        [
            (
                "text",
                string_property(
                    "Short verb-first action label for the implementation todo, anchored to its file or function when one exists; rationale, edge cases, and validation details live in section bodies",
                ),
            ),
            (
                "phase",
                string_property(
                    "Optional phase name to append this task under; omit for a flat top-level task",
                ),
            ),
        ],
        &["text"],
    ),
    execute: |store, args| {
        let mut doc = store.lock().await;
        match args.phase.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            Some(phase) => {
                let message = doc.edit().insert_task_to_phase(
                    phase,
                    &args.text,
                    InsertPosition::End,
                    None,
                )?;
                Ok(ToolOutput::Text(message))
            }
            None => {
                doc.edit().add_task_checked(&args.text)?;
                Ok(ToolOutput::Text(format!(
                    "Task added ({} tasks total).",
                    doc.tasks.len()
                )))
            }
        }
    }
);

#[cfg(test)]
plan_tool!(
    PlanAddPhaseTool,
    AddPhaseArgs,
    name: "plan_add_phase",
    description: "Add an execution phase to the plan. Use phases only for genuinely multi-stage \
         work: the implement flow runs phases one at a time and auto-advances on success, so each \
         phase's tasks must be independently runnable and ordered. Pass `tasks` to create the \
         phase and its checklist in one call; use plan_add_task with `phase` later only for \
         incremental additions. Simple fixes need no phases — keep one flat task list.",
    schema: closed_object(
        [
            (
                "name",
                string_property("Short phase name, e.g. 'Phase 1: storage' or 'Wire up handoff'"),
            ),
            (
                "tasks",
                array_property(
                    "Optional ordered checklist to create under this phase in the same call",
                    string_property(
                        "Short action label for one phase todo; put detailed context in sections",
                    ),
                ),
            ),
        ],
        &["name"],
    ),
    execute: |store, args| {
        let mut doc = store.lock().await;
        let tasks = args.tasks.unwrap_or_default();
        let task_count = doc
            .edit()
            .add_phase_with_tasks_checked(&args.name, &tasks)?;
        Ok(ToolOutput::Text(format!(
            "Phase added ({} phases total, {task_count} tasks).",
            doc.phases.len(),
        )))
    }
);

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;
    use crate::plan::PlanDoc;

    fn store() -> SharedPlanStore {
        Arc::new(Mutex::new(PlanDoc::default()))
    }

    /// Build a `PlanSetTitleTool` with an idle (no-active-session) handle.
    /// Tests that don't care about the session-title side effect can pass the
    /// returned tool straight to `.execute(...)`.
    async fn plan_title_tool(store: SharedPlanStore) -> PlanSetTitleTool {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp.path().join("bonsai.db"))
            .await
            .unwrap();
        let active_session_id: SharedActiveSessionId = Arc::new(Mutex::new(None));
        PlanSetTitleTool::new(store, storage, active_session_id)
    }

    async fn plan_replace_tool(store: SharedPlanStore) -> PlanReplaceDraftTool {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp.path().join("bonsai.db"))
            .await
            .unwrap();
        let active_session_id: SharedActiveSessionId = Arc::new(Mutex::new(None));
        PlanReplaceDraftTool::new(store, storage, active_session_id)
    }

    #[tokio::test]
    async fn set_section_and_title_update_store() {
        let store = store();
        plan_title_tool(store.clone())
            .await
            .execute(serde_json::json!({"title": "Feature"}))
            .await
            .unwrap();
        PlanSetSectionTool::new(store.clone())
            .execute(serde_json::json!({"heading": "Goal", "body": "Ship it"}))
            .await
            .unwrap();

        let doc = store.lock().await;
        assert_eq!(doc.title, "Feature");
        assert_eq!(doc.sections.len(), 1);
    }

    #[tokio::test]
    async fn replace_draft_updates_core_atomically_and_preserves_findings() {
        let store = store();
        store
            .lock()
            .await
            .edit()
            .add_finding_checked(Finding {
                severity: Severity::Major,
                file: Some("src/main.rs".to_string()),
                line: Some(7),
                issue: "old issue".to_string(),
                required_fix: "fix it".to_string(),
                acceptance_tests: Vec::new(),
                source_ids: Vec::new(),
                task: None,
                resolved: false,
            })
            .unwrap();
        let tool = plan_replace_tool(store.clone()).await;
        tool.execute(serde_json::json!({
            "title": "Fast plan",
            "episode_action": "new_topic",
            "sections": [{
                "heading": "Implementation details",
                "body": "Change the existing execution path once."
            }],
            "tasks": ["Implement change", "Run tests"],
            "phases": [],
            "questions": []
        }))
        .await
        .unwrap();

        let before_invalid = store.lock().await.clone();
        let invalid = tool
            .execute(serde_json::json!({
                "title": "Invalid",
                "sections": [],
                "tasks": ["Flat task"],
                "phases": [{"name": "Phase", "tasks": ["Phase task"]}],
                "questions": []
            }))
            .await;
        assert!(invalid.is_err());

        let plan = store.lock().await;
        assert_eq!(*plan, before_invalid, "invalid replacement must be atomic");
        assert_eq!(plan.title, "Fast plan");
        assert_eq!(plan.sections.len(), 1);
        assert_eq!(plan.tasks.len(), 2);
        assert!(plan.questions.is_empty());
        assert_eq!(plan.findings.len(), 1);
    }

    #[tokio::test]
    async fn replace_draft_schema_requires_explicit_episode_action() {
        let tool = plan_replace_tool(store()).await;
        let schema = tool.parameters_schema();

        assert_eq!(
            schema["properties"]["episode_action"]["enum"],
            serde_json::json!(["new_topic", "same_topic"])
        );
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|required| required.contains(&serde_json::json!("episode_action")))
        );
    }

    #[tokio::test]
    async fn replace_draft_requires_implementation_details_and_defaults_other_collections() {
        let store = store();
        let tool = plan_replace_tool(store.clone()).await;
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();

        assert!(required.contains(&serde_json::json!("sections")));
        for optional in ["tasks", "phases"] {
            assert!(
                !required.contains(&serde_json::json!(optional)),
                "{optional} should default to an empty collection"
            );
        }

        let missing_handoff = tool
            .execute(serde_json::json!({
                "title": "Sparse plan",
                "episode_action": "same_topic",
                "sections": [{"heading": "Overview", "body": "Change it."}],
                "tasks": ["Implement the change"]
            }))
            .await
            .unwrap_err();
        assert!(
            missing_handoff
                .to_string()
                .contains("non-empty 'Implementation details' section")
        );

        let overwritten_handoff = tool
            .execute(serde_json::json!({
                "title": "Overwritten handoff",
                "episode_action": "same_topic",
                "sections": [
                    {"heading": "Implementation details", "body": "Useful detail."},
                    {"heading": "implementation DETAILS", "body": ""}
                ],
                "tasks": ["Implement the change"]
            }))
            .await
            .unwrap_err();
        assert!(
            overwritten_handoff
                .to_string()
                .contains("non-empty 'Implementation details' section")
        );

        tool.execute(serde_json::json!({
            "title": "Flat plan",
            "episode_action": "same_topic",
            "sections": [{
                "heading": "implementation DETAILS",
                "body": "Update the existing execution path."
            }],
            "tasks": ["Implement the change"]
        }))
        .await
        .unwrap();

        let plan = store.lock().await;
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.sections.len(), 1);
        assert!(plan.phases.is_empty());
        assert!(plan.questions.is_empty());
    }

    #[tokio::test]
    async fn replace_draft_rejects_open_questions() {
        let store = store();
        let tool = plan_replace_tool(store).await;

        let error = tool
            .execute(serde_json::json!({
                "title": "Blocked plan",
                "episode_action": "same_topic",
                "tasks": ["Implement the change"],
                "questions": ["Which behavior?"]
            }))
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("ask the user with the question tool")
        );
        assert!(
            tool.parameters_schema()["properties"]
                .get("questions")
                .is_none()
        );
    }

    #[tokio::test]
    async fn add_and_check_task_round_trip() {
        let store = store();
        PlanAddTaskTool::new(store.clone())
            .execute(serde_json::json!({"text": "Write tests"}))
            .await
            .unwrap();
        let result = PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "check", "target": "tests"}))
            .await
            .unwrap();

        assert!(matches!(result, ToolOutput::Text(text) if text.contains("Write tests")));
        assert!(store.lock().await.tasks[0].done);
    }

    #[tokio::test]
    async fn patch_section_appends_and_replaces_text() {
        let store = store();
        PlanPatchSectionTool::new(store.clone())
            .execute(serde_json::json!({
                "heading": "Approach",
                "operation": "replace",
                "text": "First"
            }))
            .await
            .unwrap();
        PlanPatchSectionTool::new(store.clone())
            .execute(serde_json::json!({
                "heading": "Approach",
                "operation": "append",
                "text": "Second"
            }))
            .await
            .unwrap();
        let result = PlanPatchSectionTool::new(store.clone())
            .execute(serde_json::json!({
                "heading": "Approach",
                "operation": "replace_text",
                "old_text": "Second",
                "new_text": "Done"
            }))
            .await
            .unwrap();

        assert!(matches!(result, ToolOutput::Text(text) if text.contains("Patched")));
        assert_eq!(store.lock().await.sections[0].body, "First\n\nDone");
    }

    #[tokio::test]
    async fn patch_section_validates_required_text() {
        let store = store();
        let result = PlanPatchSectionTool::new(store)
            .execute(serde_json::json!({
                "heading": "Approach",
                "operation": "append"
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn insert_update_remove_and_uncheck_task_tools_mutate_store() {
        let store = store();
        PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "insert", "text": "B", "position": "end"}))
            .await
            .unwrap();
        PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "insert", "text": "A", "position": "before", "target": "B"}))
            .await
            .unwrap();
        PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "update", "target": "B", "text": "B revised"}))
            .await
            .unwrap();
        PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "check", "target": "revised"}))
            .await
            .unwrap();
        let unchecked = PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "uncheck", "target": "revised"}))
            .await
            .unwrap();
        let removed = PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "remove", "target": "A"}))
            .await
            .unwrap();

        assert!(matches!(unchecked, ToolOutput::Text(text) if text.contains("B revised")));
        assert!(matches!(removed, ToolOutput::Text(text) if text.contains("A")));
        let doc = store.lock().await;
        assert_eq!(doc.tasks.len(), 1);
        assert_eq!(doc.tasks[0].text, "B revised");
        assert!(!doc.tasks[0].done);
    }

    #[tokio::test]
    async fn insert_task_validates_relative_target() {
        let store = store();
        let result = PlanTasksTool::new(store)
            .execute(serde_json::json!({"operation": "insert", "text": "A", "position": "before"}))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn add_phase_accepts_initial_tasks() {
        let store = store();
        let result = PlanAddPhaseTool::new(store.clone())
            .execute(serde_json::json!({
                "name": "Phase 1: storage",
                "tasks": ["add table", "wire repository"]
            }))
            .await
            .unwrap();

        assert!(matches!(result, ToolOutput::Text(text) if text.contains("2 tasks")));
        let doc = store.lock().await;
        assert_eq!(doc.phases.len(), 1);
        assert_eq!(doc.phases[0].name, "Phase 1: storage");
        let tasks: Vec<&str> = doc.phases[0]
            .tasks
            .iter()
            .map(|task| task.text.as_str())
            .collect();
        assert_eq!(tasks, ["add table", "wire repository"]);
    }

    #[test]
    fn add_phase_schema_exposes_tasks_array() {
        let schema = PlanAddPhaseTool::new(store()).parameters_schema();

        assert_eq!(
            schema
                .pointer("/properties/tasks/type")
                .and_then(serde_json::Value::as_str),
            Some("array")
        );
        assert_eq!(
            schema
                .pointer("/properties/tasks/items/type")
                .and_then(serde_json::Value::as_str),
            Some("string")
        );
    }

    #[tokio::test]
    async fn remove_section_errors_on_miss() {
        let store = store();
        let result = PlanPatchSectionTool::new(store)
            .execute(serde_json::json!({"operation": "remove", "heading": "Ghost"}))
            .await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No plan section"), "got: {err}");
    }

    #[tokio::test]
    async fn task_tools_error_on_miss() {
        // A genuine miss (target absent) must be an unambiguous error for every
        // plan tool, not a success-looking no-op the model could misread.
        let store = store();

        let check = PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "check", "target": "ghost"}))
            .await;
        assert!(check.unwrap_err().to_string().contains("No unchecked task"));

        let update = PlanTasksTool::new(store.clone())
            .execute(
                serde_json::json!({"operation": "update", "target": "ghost", "text": "revised"}),
            )
            .await;
        assert!(update.unwrap_err().to_string().contains("No task matching"));

        let remove = PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "remove", "target": "ghost"}))
            .await;
        assert!(remove.unwrap_err().to_string().contains("No task matching"));

        let remove_section = PlanPatchSectionTool::new(store)
            .execute(serde_json::json!({"operation": "remove", "heading": "ghost"}))
            .await;
        assert!(
            remove_section
                .unwrap_err()
                .to_string()
                .contains("No plan section")
        );
    }

    #[tokio::test]
    async fn tasks_tool_moves_and_removes_phases() {
        let store = store();
        for name in ["Storage", "Wiring"] {
            PlanAddPhaseTool::new(store.clone())
                .execute(serde_json::json!({"name": name, "tasks": ["step"]}))
                .await
                .unwrap();
        }
        PlanTasksTool::new(store.clone())
            .execute(
                serde_json::json!({"operation": "insert", "text": "Add index", "phase": "Storage"}),
            )
            .await
            .unwrap();
        PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "move_phase", "phase": "Wiring", "position": "start"}))
            .await
            .unwrap();
        {
            let doc = store.lock().await;
            let names: Vec<&str> = doc.phases.iter().map(|phase| phase.name.as_str()).collect();
            assert_eq!(names, ["Wiring", "Storage"]);
            assert_eq!(doc.phases[1].tasks.len(), 2);
        }
        PlanTasksTool::new(store.clone())
            .execute(serde_json::json!({"operation": "remove_phase", "phase": "Wiring"}))
            .await
            .unwrap();
        assert_eq!(store.lock().await.phases.len(), 1);

        let miss = PlanTasksTool::new(store)
            .execute(serde_json::json!({"operation": "remove_phase", "phase": "Ghost"}))
            .await;
        assert!(miss.unwrap_err().to_string().contains("No plan phase"));
    }

    #[tokio::test]
    async fn consolidated_tools_name_missing_operands() {
        let store = store();
        let cases = [
            (
                PlanTasksTool::new(store.clone())
                    .execute(serde_json::json!({"operation": "update", "target": "x"}))
                    .await,
                "text must not be empty",
            ),
            (
                PlanTasksTool::new(store.clone())
                    .execute(serde_json::json!({"operation": "check"}))
                    .await,
                "target must not be empty",
            ),
            (
                PlanTasksTool::new(store.clone())
                    .execute(serde_json::json!({"operation": "move_phase", "phase": "P"}))
                    .await,
                "move_phase requires position",
            ),
            (
                PlanPatchSectionTool::new(store.clone())
                    .execute(serde_json::json!({"heading": "Goal", "operation": "move"}))
                    .await,
                "move requires position",
            ),
            (
                PlanFindingsTool::new(store.clone())
                    .execute(
                        serde_json::json!({"operation": "add", "severity": "major", "issue": "x"}),
                    )
                    .await,
                "required_fix must not be empty",
            ),
            (
                PlanFindingsTool::new(store)
                    .execute(serde_json::json!({"operation": "resolve"}))
                    .await,
                "finding must not be empty",
            ),
        ];
        for (result, expected) in cases {
            let err = result.expect_err(expected).to_string();
            assert!(err.contains(expected), "expected {expected:?}, got {err:?}");
        }
    }

    #[test]
    fn planning_surface_has_at_most_six_plan_tools() {
        let names: Vec<&str> = crate::tool::profile::descriptors_for(
            crate::tool::profile::ToolProfile::Planning,
            crate::tool::profile::RegistrationStage::Standard,
        )
        .into_iter()
        .map(|descriptor| descriptor.name)
        .filter(|name| name.starts_with("plan_"))
        .collect();
        assert_eq!(
            names,
            [
                "plan_replace_draft",
                "plan_patch_section",
                "plan_tasks",
                "plan_findings"
            ]
        );
    }

    #[tokio::test]
    async fn uncheck_task_errors_on_miss() {
        let store = store();
        let result = PlanTasksTool::new(store)
            .execute(serde_json::json!({"operation": "uncheck", "target": "ghost"}))
            .await;
        assert!(result.unwrap_err().to_string().contains("No checked task"));
    }

    #[test]
    fn plan_tools_use_serialized_parallel_policy() {
        // The plan tools mutate a single shared canvas, so the macro must keep
        // them Serialized rather than regressing to a path-scoped default.
        use crate::tool::ParallelPolicy;
        let store = store();
        assert_eq!(
            PlanAddTaskTool::new(store.clone()).parallel_policy(),
            ParallelPolicy::Serialized
        );
        assert_eq!(
            PlanPatchSectionTool::new(store).parallel_policy(),
            ParallelPolicy::Serialized
        );
    }

    #[tokio::test]
    async fn rejects_empty_title_and_task() {
        let store = store();
        assert!(
            plan_title_tool(store.clone())
                .await
                .execute(serde_json::json!({"title": "  "}))
                .await
                .is_err()
        );
        assert!(
            PlanAddTaskTool::new(store)
                .execute(serde_json::json!({"text": ""}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn plan_set_title_rejects_overlong_title() {
        // Mirrors `set_session_title`'s 80-char cap so the mirrored session
        // summary can't carry more than that tool allows.
        let tool = plan_title_tool(store()).await;
        let err = tool
            .execute(serde_json::json!({"title": "x".repeat(81)}))
            .await
            .expect_err("titles longer than 80 chars should be rejected");
        assert!(
            err.to_string().contains("80 characters or fewer"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn set_title_mirrors_to_active_session() {
        // Renaming the plan must update the session summary too so the TUI header
        // and session pickers don't drift out of sync.
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp.path().join("bonsai.db"))
            .await
            .unwrap();
        let session_id = storage
            .start_session(
                temp.path(),
                "codex",
                "test-model",
                crate::provider::ReasoningSelection::default(),
            )
            .await
            .unwrap();
        let active_session_id: SharedActiveSessionId = Arc::new(Mutex::new(Some(session_id)));
        let tool = PlanSetTitleTool::new(store(), storage.clone(), active_session_id);

        tool.execute(serde_json::json!({"title": "Refactor editor"}))
            .await
            .unwrap();

        let sessions = storage
            .recent_sessions_for_project(temp.path(), 10)
            .await
            .unwrap();
        assert_eq!(sessions[0].summary, "Refactor editor");
    }

    #[tokio::test]
    async fn set_title_with_no_active_session_just_updates_plan() {
        // No active persisted session must not fail the plan update — the plan is
        // the source of truth for tests/eval/headless replays.
        let tool = plan_title_tool(store()).await;
        tool.execute(serde_json::json!({"title": "Refactor editor"}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn move_section_tool_reorders_store() {
        let store = store();
        for heading in ["Goal", "Approach", "Risks"] {
            PlanSetSectionTool::new(store.clone())
                .execute(serde_json::json!({"heading": heading, "body": "x"}))
                .await
                .unwrap();
        }

        PlanPatchSectionTool::new(store.clone())
            .execute(
                serde_json::json!({"operation": "move", "heading": "Risks", "position": "start"}),
            )
            .await
            .unwrap();

        let headings: Vec<String> = store
            .lock()
            .await
            .sections
            .iter()
            .map(|section| section.heading.clone())
            .collect();
        assert_eq!(headings, ["Risks", "Goal", "Approach"]);
    }

    #[tokio::test]
    async fn move_section_tool_errors_on_missing_target() {
        let store = store();
        PlanSetSectionTool::new(store.clone())
            .execute(serde_json::json!({"heading": "Goal", "body": "x"}))
            .await
            .unwrap();
        let result = PlanPatchSectionTool::new(store)
            .execute(
                serde_json::json!({"operation": "move", "heading": "Goal", "position": "before", "target": "Ghost"}),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("No plan section"));
    }

    #[tokio::test]
    async fn add_finding_records_structured_finding() {
        let store = store();
        PlanFindingsTool::new(store.clone())
            .execute(serde_json::json!({"operation": "add",
                "severity": "blocker",
                "file": "src/foo.rs",
                "line": 42,
                "issue": "null deref",
                "required_fix": "guard the option",
                "acceptance_tests": ["test_guard passes"],
                "source_ids": ["call-7"],
            }))
            .await
            .unwrap();
        let doc = store.lock().await;
        assert_eq!(doc.findings.len(), 1);
        let finding = &doc.findings[0];
        assert_eq!(finding.severity, Severity::Blocker);
        assert_eq!(finding.line, Some(42));
        assert_eq!(finding.source_ids, vec!["call-7".to_string()]);
    }

    #[tokio::test]
    async fn add_finding_rejects_invalid_severity() {
        let store = store();
        let err = PlanFindingsTool::new(store)
            .execute(serde_json::json!({"operation": "add",
                "severity": "catastrophic",
                "issue": "x",
                "required_fix": "y",
            }))
            .await;
        assert!(err.unwrap_err().to_string().contains("invalid severity"));
    }

    #[tokio::test]
    async fn associate_and_resolve_finding_round_trip() {
        let store = store();
        PlanAddTaskTool::new(store.clone())
            .execute(serde_json::json!({"text": "Patch the parser"}))
            .await
            .unwrap();
        PlanFindingsTool::new(store.clone())
            .execute(serde_json::json!({"operation": "add",
                "severity": "major",
                "issue": "parser regression",
                "required_fix": "restore the branch",
            }))
            .await
            .unwrap();
        PlanFindingsTool::new(store.clone())
            .execute(serde_json::json!({"operation": "associate", "finding": "regression", "task": "parser"}))
            .await
            .unwrap();
        assert_eq!(
            store.lock().await.findings[0].task.as_deref(),
            Some("Patch the parser")
        );

        PlanFindingsTool::new(store.clone())
            .execute(serde_json::json!({"operation": "resolve", "finding": "regression"}))
            .await
            .unwrap();
        let doc = store.lock().await;
        assert_eq!(doc.findings.len(), 1, "resolve keeps the record");
        assert!(doc.findings[0].resolved);
    }
}
