//! `memory_write` — the auto-capture surface for persistent memory.
//! The user tier lives outside the project root where `write`/`edit` refuse to
//! go, so this tool is the only model-facing write path there; project-tier
//! files stay editable through the normal file tools too. Saves render as a
//! normal file-change diff in the TUI, which is the transparency for the
//! capture being auto-approved (memory dirs only, markdown only).

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

use crate::diff::build_file_diff;
use crate::memory::MemoryService;
use crate::memory::entry::{MemoryEntryType, MemoryTier};
use crate::tool::file_mutation::build_mutation_output;
use crate::tool::schema::{object, parse_args, string_enum_property, string_property};
use crate::tool::{Tool, ToolOutput};

/// Two same-tier descriptions at or above this `normalized_levenshtein`
/// similarity are treated as the same fact: the save is rejected with the
/// existing entry's name so the model updates instead of duplicating. Bounds
/// store growth against near-identical re-captures.
const NEAR_DUPLICATE_SIMILARITY: f64 = 0.75;

#[derive(Deserialize)]
struct MemoryWriteArgs {
    action: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default, rename = "type")]
    entry_type: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

pub struct MemoryWriteTool {
    memory: Arc<MemoryService>,
}

impl MemoryWriteTool {
    pub(crate) fn new(memory: Arc<MemoryService>) -> Self {
        Self { memory }
    }

    fn save(&self, args: MemoryWriteArgs) -> Result<ToolOutput> {
        let tier = match args.tier.as_deref() {
            Some("user") => MemoryTier::User,
            Some("project") => MemoryTier::Project,
            Some(other) => bail!("unknown tier {other:?} (expected user or project)"),
            None => bail!("save needs a tier: user (follows you) or project (ships with repo)"),
        };
        let entry_type = match args.entry_type.as_deref() {
            Some("user") => MemoryEntryType::User,
            Some("preference") => MemoryEntryType::Preference,
            Some("project") => MemoryEntryType::Project,
            Some("reference") => MemoryEntryType::Reference,
            Some(other) => {
                bail!("unknown type {other:?} (expected user, preference, project, or reference)")
            }
            None => bail!("save needs a type: user, preference, project, or reference"),
        };
        let description = args
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .ok_or_else(|| anyhow::anyhow!("save needs a one-line description"))?;
        let body = args
            .body
            .as_deref()
            .map(str::trim)
            .filter(|body| !body.is_empty())
            .ok_or_else(|| anyhow::anyhow!("save needs a body (the fact itself)"))?;
        let name = args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty());

        let store = self.memory.store();
        // Updating an existing entry by name skips the near-dup guard — that's
        // exactly the "update instead of duplicating" path.
        let updating = name.is_some_and(|name| {
            store
                .entries()
                .iter()
                .any(|entry| entry.tier == tier && entry.name == name)
        });
        if !updating
            && let Some(existing) = store.entries().iter().find(|entry| {
                entry.tier == tier
                    && strsim::normalized_levenshtein(&entry.description, description)
                        >= NEAR_DUPLICATE_SIMILARITY
            })
        {
            bail!(
                "an entry already covers this: {:?} — {}. Pass name: {:?} to update it instead of duplicating.",
                existing.name,
                existing.description,
                existing.name,
            );
        }

        let written = store.write(tier, entry_type, name, description, body, None)?;
        self.memory.note_captured(&written.entry.name);
        let summary = if written.created {
            format!("remembered: {}", written.entry.description)
        } else {
            format!("updated memory: {}", written.entry.description)
        };
        let diff = build_file_diff(
            written.entry.path.display().to_string(),
            written.previous.as_deref(),
            &crate::memory::entry::render_memory_file(&written.entry),
        );
        Ok(build_mutation_output(diff, |_| summary))
    }

    async fn forget(&self, args: MemoryWriteArgs) -> Result<ToolOutput> {
        let name = args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("forget needs the entry name"))?;
        let path = self.memory.forget(name).await?;
        Ok(ToolOutput::Text(format!(
            "forgot memory entry {name} ({})",
            path.display()
        )))
    }
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "memory_write"
    }

    fn description(&self) -> &str {
        "Save a durable fact to persistent memory, update one, or forget one proven wrong. \
         Save at the moment you learn it, when one of these happens: the user corrects you or \
         states a standing preference (save the rule and the why); the user explicitly asks you \
         to remember something; you discover a non-obvious fact the hard way (environment quirk, \
         flaky test, tool/provider gotcha) that would cost the same effort to rediscover; a \
         project goal, constraint, or decision plus its rationale is settled but not recorded in \
         the repo; an external dashboard/ticket/doc keeps coming up (type reference). \
         NEVER save: instructions scoped to the current task only (standing means it outlives \
         this session), secrets or credentials, unverified guesses, or anything the repo, git \
         history, or steering files (AGENTS.md/CLAUDE.md) already record — if the user asks to \
         remember one of those, save what was non-obvious about it instead. \
         Tier user follows the user across projects; tier project is git-tracked and visible to \
         everyone who clones this repo, so personal or sensitive facts always go in tier user. \
         Write dates absolute (2026-07-18), never relative (next week). \
         When an existing memory entry already covers the topic, pass its name to update that \
         entry instead of duplicating; when the user reverses a saved preference, update or \
         forget the old entry rather than adding a contradicting sibling."
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [
                (
                    "action",
                    string_enum_property("save (create or update) | forget", &["save", "forget"]),
                ),
                (
                    "tier",
                    string_enum_property(
                        "save: where the fact lives — user (follows the user across projects) or project (git-tracked, ships with this repo)",
                        &["user", "project"],
                    ),
                ),
                (
                    "type",
                    string_enum_property(
                        "save: user (who they are) | preference (standing preference/correction) | project (goal/constraint/decision) | reference (external pointer)",
                        &["user", "preference", "project", "reference"],
                    ),
                ),
                (
                    "name",
                    string_property(
                        "Entry name (kebab-case). Pass an existing name to update it; required for forget; omit to derive from the description",
                    ),
                ),
                (
                    "description",
                    string_property("save: one line for the memory index, max 160 chars"),
                ),
                (
                    "body",
                    string_property(
                        "save: the fact, why it holds, and how to apply it — markdown, a few lines at most",
                    ),
                ),
            ],
            &["action"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: MemoryWriteArgs = parse_args("memory_write", args)?;
        match args.action.as_str() {
            "save" => self.save(args),
            "forget" => self.forget(args).await,
            other => bail!("unknown action {other:?} (expected save or forget)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn service() -> (tempfile::TempDir, tempfile::TempDir, Arc<MemoryService>) {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let db = tempfile::TempDir::new().unwrap();
        let storage = crate::storage::Storage::open_at(db.path().join("bonsai.db"))
            .await
            .unwrap();
        let memory = Arc::new(MemoryService::load(home.path(), project.path(), storage, 0));
        (home, project, memory)
    }

    fn save_args(description: &str) -> serde_json::Value {
        serde_json::json!({
            "action": "save",
            "tier": "user",
            "type": "preference",
            "description": description,
            "body": "Details here.",
        })
    }

    #[tokio::test]
    async fn description_covers_capture_corner_cases() {
        let (_home, _project, memory) = service().await;
        let tool = MemoryWriteTool::new(memory);
        let description = tool.description();
        // The when-to-save decision procedure lives here; these are the corner
        // cases the model must know without re-deriving them.
        for phrase in [
            "the moment you learn it",
            "explicitly asks you to remember",
            "the hard way",
            "save the rule and the why",
            "standing means it outlives this session",
            "secrets or credentials",
            "save what was non-obvious about it instead",
            "visible to everyone who clones this repo",
            "never relative",
            "update or forget the old entry",
        ] {
            assert!(description.contains(phrase), "missing: {phrase}");
        }
    }

    #[tokio::test]
    async fn save_renders_created_diff_with_remembered_summary() {
        let (_home, _project, memory) = service().await;
        let tool = MemoryWriteTool::new(memory.clone());
        let output = tool
            .execute(save_args("Run tests with --quiet"))
            .await
            .unwrap();
        match output {
            ToolOutput::Edit { summary, diff } => {
                assert_eq!(summary, "remembered: Run tests with --quiet");
                assert!(diff.path.ends_with("run-tests-with-quiet.md"));
                assert_eq!(diff.removed_lines, 0);
                assert!(diff.added_lines > 0);
            }
            other => panic!("expected an edit output, got {other:?}"),
        }
        assert_eq!(memory.store().entries().len(), 1);
    }

    #[tokio::test]
    async fn near_duplicate_save_names_the_existing_entry() {
        let (_home, _project, memory) = service().await;
        let tool = MemoryWriteTool::new(memory);
        tool.execute(save_args("Run the tests with --quiet flag"))
            .await
            .unwrap();
        let err = tool
            .execute(save_args("Run the tests with the --quiet flag"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("run-the-tests-with-quiet-flag"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn update_by_name_skips_near_dup_guard_and_diffs_previous() {
        let (_home, _project, memory) = service().await;
        let tool = MemoryWriteTool::new(memory);
        tool.execute(save_args("Run tests with --quiet"))
            .await
            .unwrap();
        let output = tool
            .execute(serde_json::json!({
                "action": "save",
                "tier": "user",
                "type": "preference",
                "name": "run-tests-with-quiet",
                "description": "Run tests with --quiet always",
                "body": "Updated detail.",
            }))
            .await
            .unwrap();
        match output {
            ToolOutput::Edit { summary, diff } => {
                assert_eq!(summary, "updated memory: Run tests with --quiet always");
                assert!(
                    diff.removed_lines > 0,
                    "update should diff the previous file"
                );
            }
            other => panic!("expected an edit output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forget_deletes_and_reports_path() {
        let (home, _project, memory) = service().await;
        let tool = MemoryWriteTool::new(memory);
        tool.execute(save_args("Run tests with --quiet"))
            .await
            .unwrap();
        let output = tool
            .execute(serde_json::json!({"action": "forget", "name": "run-tests-with-quiet"}))
            .await
            .unwrap();
        match output {
            ToolOutput::Text(text) => assert!(text.starts_with("forgot memory entry")),
            other => panic!("expected text output, got {other:?}"),
        }
        assert!(!home.path().join("memory/run-tests-with-quiet.md").exists());
        let err = tool
            .execute(serde_json::json!({"action": "forget", "name": "run-tests-with-quiet"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no memory entry"), "{err:#}");
    }

    #[tokio::test]
    async fn save_requires_tier_type_description_body() {
        let (_home, _project, memory) = service().await;
        let tool = MemoryWriteTool::new(memory);
        for missing in ["tier", "type", "description", "body"] {
            let mut args = save_args("A fact");
            args.as_object_mut().unwrap().remove(missing);
            assert!(
                tool.execute(args).await.is_err(),
                "save without {missing} should fail"
            );
        }
    }
}
