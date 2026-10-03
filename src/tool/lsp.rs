use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::lsp::{self, LspHub, LspLocation, LspSymbol};
use crate::tool::file_mutation::{
    FileMutationAction, FileMutationContext, FileWriteRequest, ParentDirs, WriteContentContext,
    WritePrecondition, restore_file_content,
};
use crate::tool::output::cap_text;
use crate::tool::schema::{
    boolean_property, bounded_integer_property, closed_object, parse_args, path_property,
    string_property,
};
use crate::tool::{
    ActionEffect, ActionPlan, ActionPolicy, MutationTarget, ParallelPolicy, ReadTracker, Tool,
    ToolOutput, WorkspaceLockContext,
};
use crate::yolo::YoloMode;

const MAX_OUTPUT_CHARS: usize = 20_000;

#[derive(Debug, Deserialize)]
struct PositionArgs {
    path: String,
    line: u32,
    character: u32,
    #[serde(default)]
    recheck: bool,
}

#[derive(Debug, Deserialize)]
struct ReferencesArgs {
    path: String,
    line: u32,
    character: u32,
    #[serde(default)]
    include_declaration: bool,
    #[serde(default)]
    recheck: bool,
}

#[derive(Debug, Deserialize)]
struct WorkspaceSymbolArgs {
    path: String,
    query: String,
    #[serde(default)]
    recheck: bool,
}

#[derive(Debug, Deserialize)]
struct RenameArgs {
    path: String,
    line: u32,
    character: u32,
    new_name: String,
}

pub(crate) struct DefinitionTool {
    hub: Arc<LspHub>,
}

impl DefinitionTool {
    pub(crate) fn new(hub: Arc<LspHub>) -> Self {
        Self { hub }
    }
}

pub(crate) struct ReferencesTool {
    hub: Arc<LspHub>,
}

impl ReferencesTool {
    pub(crate) fn new(hub: Arc<LspHub>) -> Self {
        Self { hub }
    }
}

pub(crate) struct HoverTool {
    hub: Arc<LspHub>,
}

impl HoverTool {
    pub(crate) fn new(hub: Arc<LspHub>) -> Self {
        Self { hub }
    }
}

pub(crate) struct WorkspaceSymbolTool {
    hub: Arc<LspHub>,
}

impl WorkspaceSymbolTool {
    pub(crate) fn new(hub: Arc<LspHub>) -> Self {
        Self { hub }
    }
}

pub(crate) struct RenameSymbolTool {
    hub: Arc<LspHub>,
    ctx: FileMutationContext,
}

impl RenameSymbolTool {
    pub(crate) fn with_hooks_and_locks(
        project_root: PathBuf,
        hub: Arc<LspHub>,
        read_tracker: ReadTracker,
        yolo_mode: YoloMode,
        hooks: Arc<crate::hooks::HookEngine>,
        workspace_locks: WorkspaceLockContext,
        policy: ActionPolicy,
    ) -> Self {
        Self {
            hub,
            ctx: FileMutationContext::new_with_locks(
                project_root,
                read_tracker,
                yolo_mode,
                hooks,
                workspace_locks,
                policy,
            ),
        }
    }
}

fn position_schema() -> serde_json::Value {
    closed_object(
        [
            (
                "path",
                path_property("File path relative to project root, or absolute path inside it"),
            ),
            (
                "line",
                bounded_integer_property("1-based line number", Some(1), None),
            ),
            (
                "character",
                bounded_integer_property(
                    "1-based character position on the line, using LSP/UTF-16 columns",
                    Some(1),
                    None,
                ),
            ),
            (
                "recheck",
                boolean_property(
                    "Bypass cached missing-path evidence and check the filesystem again",
                ),
            ),
        ],
        &["path", "line", "character"],
    )
}

#[async_trait]
impl Tool for DefinitionTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "definition"
    }

    fn description(&self) -> &str {
        "Use the language server to find the definition of the symbol at path, line, and character. Supports any registered LSP; Rust uses rust-analyzer."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        position_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: PositionArgs = parse_args("definition", args)?;
        let locations = match self
            .hub
            .definition_with_recheck(&args.path, args.line, args.character, args.recheck)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(fallback) = recovery_fallback(&error) {
                    return Ok(fallback);
                }
                return Err(error.into());
            }
        };
        let text = format_locations(
            self.hub.project_root(),
            &locations.value,
            "No definition found.",
        );
        Ok(ToolOutput::Text(cap_text(
            with_recovery_notice(text, locations.recovery_notice),
            MAX_OUTPUT_CHARS,
            "Use read on the most relevant file.",
        )))
    }
}

#[async_trait]
impl Tool for ReferencesTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "references"
    }

    fn description(&self) -> &str {
        "Use the language server to find references to the symbol at path, line, and character. Set include_declaration=true to include the definition."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "path",
                    path_property("File path relative to project root, or absolute path inside it"),
                ),
                (
                    "line",
                    bounded_integer_property("1-based line number", Some(1), None),
                ),
                (
                    "character",
                    bounded_integer_property(
                        "1-based character position on the line, using LSP/UTF-16 columns",
                        Some(1),
                        None,
                    ),
                ),
                (
                    "include_declaration",
                    boolean_property("Whether to include the declaration/definition"),
                ),
                (
                    "recheck",
                    boolean_property(
                        "Bypass cached missing-path evidence and check the filesystem again",
                    ),
                ),
            ],
            &["path", "line", "character"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ReferencesArgs = parse_args("references", args)?;
        let locations = match self
            .hub
            .references_with_recheck(
                &args.path,
                args.line,
                args.character,
                args.include_declaration,
                args.recheck,
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) if likely_warming_error(&error) => {
                return Ok(ToolOutput::Text(
                    "Language server is still warming; retry references in 3s. Use grep meanwhile if the result is urgent."
                        .to_string(),
                ));
            }
            Err(error) => {
                if let Some(fallback) = recovery_fallback(&error) {
                    return Ok(fallback);
                }
                return Err(error.into());
            }
        };
        if locations.value.is_empty() {
            return Ok(ToolOutput::Text(with_recovery_notice(
                "No references found. If this is the server's first request, it may still be indexing; retry in 3s or use grep."
                    .to_string(),
                locations.recovery_notice,
            )));
        }
        Ok(ToolOutput::Text(cap_text(
            with_recovery_notice(
                format_locations(
                    self.hub.project_root(),
                    &locations.value,
                    "No references found.",
                ),
                locations.recovery_notice,
            ),
            MAX_OUTPUT_CHARS,
            "Use query/path filters or read the referenced files.",
        )))
    }
}

fn with_recovery_notice(text: String, notice: Option<String>) -> String {
    notice.map_or(text.clone(), |notice| format!("{notice}\n\n{text}"))
}

fn recovery_fallback(error: &lsp::LspError) -> Option<ToolOutput> {
    if let lsp::LspError::ReusedMissingPath { evidence } = error {
        return Some(ToolOutput::MissingPathReuse {
            text: evidence.render_reuse(),
        });
    }
    matches!(error, lsp::LspError::RecoveryUnavailable { .. }).then(|| {
        ToolOutput::Text(format!(
            "{error}\nContinue with built-in symbol_search/grep while LSP recovery cools down."
        ))
    })
}

fn likely_warming_error(error: &lsp::LspError) -> bool {
    match error {
        lsp::LspError::Protocol(message) => {
            let message = message.to_ascii_lowercase();
            message.contains("timed out")
                || message.contains("cancelled")
                || message.contains("not initialized")
                || message.contains("server is not ready")
        }
        lsp::LspError::Server(error) => {
            let message = error.message.to_ascii_lowercase();
            message.contains("not initialized")
                || message.contains("not ready")
                || message.contains("indexing")
        }
        _ => false,
    }
}

#[async_trait]
impl Tool for HoverTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "hover"
    }

    fn description(&self) -> &str {
        "Use the language server to return hover/type information for the symbol at path, line, and character."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        position_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: PositionArgs = parse_args("hover", args)?;
        let hover = match self
            .hub
            .hover_with_recheck(&args.path, args.line, args.character, args.recheck)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(fallback) = recovery_fallback(&error) {
                    return Ok(fallback);
                }
                return Err(error.into());
            }
        };
        let recovery_notice = hover.recovery_notice;
        let Some(hover) = hover.value else {
            return Ok(ToolOutput::Text(with_recovery_notice(
                "No hover information found.".to_string(),
                recovery_notice,
            )));
        };
        let mut out = hover.contents;
        if let Some(range) = hover.range {
            out.push_str(&format!(
                "\n\nrange: {}:{}-{}:{}",
                range.start.line + 1,
                range.start.character + 1,
                range.end.line + 1,
                range.end.character + 1
            ));
        }
        Ok(ToolOutput::Text(cap_text(
            with_recovery_notice(out, recovery_notice),
            MAX_OUTPUT_CHARS,
            "Use definition or references for more context.",
        )))
    }
}

#[async_trait]
impl Tool for WorkspaceSymbolTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "workspace_symbol"
    }

    fn description(&self) -> &str {
        "Use the language server to search workspace symbols. Provide a path in the target language so the correct server/workspace is selected."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "path",
                    path_property(
                        "A file or directory inside the project used to choose the language server",
                    ),
                ),
                ("query", string_property("Symbol query")),
                (
                    "recheck",
                    boolean_property(
                        "Bypass cached missing-path evidence and check the filesystem again",
                    ),
                ),
            ],
            &["path", "query"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: WorkspaceSymbolArgs = parse_args("workspace_symbol", args)?;
        let symbols = match self
            .hub
            .workspace_symbol_with_recheck(&args.path, &args.query, args.recheck)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(fallback) = recovery_fallback(&error) {
                    return Ok(fallback);
                }
                return Err(error.into());
            }
        };
        Ok(ToolOutput::Text(cap_text(
            with_recovery_notice(
                format_symbols(self.hub.project_root(), &symbols.value),
                symbols.recovery_notice,
            ),
            MAX_OUTPUT_CHARS,
            "Use a narrower query.",
        )))
    }
}

#[async_trait]
impl Tool for RenameSymbolTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::SelfAuthorized
    }

    fn name(&self) -> &str {
        "rename_symbol"
    }

    fn description(&self) -> &str {
        "Rename the symbol at path, line, and character using the language server. Applies only in-project edits and requires every touched file to have been read and still be unchanged since that read."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "path",
                    path_property("File path relative to project root, or absolute path inside it"),
                ),
                (
                    "line",
                    bounded_integer_property("1-based line number", Some(1), None),
                ),
                (
                    "character",
                    bounded_integer_property(
                        "1-based character position on the line, using LSP/UTF-16 columns",
                        Some(1),
                        None,
                    ),
                ),
                ("new_name", string_property("New symbol name")),
            ],
            &["path", "line", "character", "new_name"],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::Serialized
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: RenameArgs = parse_args("rename_symbol", args)?;
        self.hub
            .prepare_rename(&args.path, args.line, args.character)
            .await?;
        let edit = self
            .hub
            .rename(&args.path, args.line, args.character, &args.new_name)
            .await?;
        if edit.changes.is_empty() {
            anyhow::bail!("language server returned no edits for rename_symbol");
        }
        let changed_paths = lsp::workspace_edit_paths(&edit)?;
        for path in &changed_paths {
            self.hub
                .resolve_existing_project_file(&path.display().to_string(), "rename symbol")?;
        }
        let edited = crate::lsp::plan_workspace_edit(
            self.hub.project_root(),
            self.ctx.read_tracker(),
            &edit,
        )
        .await?;
        if edited.is_empty() {
            anyhow::bail!("rename_symbol made no changes");
        }
        let planned_targets = edited
            .iter()
            .map(|file| MutationTarget::new(&file.relative_path, Path::new(&file.relative_path)))
            .collect::<Vec<_>>();
        let plan = ActionPlan::file_mutation(
            "rename_symbol",
            planned_targets.iter(),
            [ActionEffect::WorkspaceWrite],
        );
        let guard = self.ctx.guard(FileMutationAction::Edit);
        guard.authorize(&plan).await?;
        let workspace_lock = guard.acquire_write_lock().await?;
        let mut preflighted = Vec::with_capacity(edited.len());
        for file in &edited {
            let preflight =
                guard.preflight_file_write("rename_symbol", &file.relative_path, &file.diff);
            preflighted.push(match workspace_lock.as_ref() {
                Some(workspace_lock) => workspace_lock.run_while_held(preflight).await?,
                None => preflight.await?,
            });
        }

        let mut rollbacks: Vec<(&std::path::Path, &str, &str)> = Vec::with_capacity(edited.len());
        for (file, preflighted) in edited.iter().zip(&preflighted) {
            let request = FileWriteRequest::new(
                &file.path,
                &file.new_content,
                WritePrecondition::Exact(&file.old_content),
                WriteContentContext::WriteFile,
                ParentDirs::Keep,
            )
            .with_workspace_lock(workspace_lock.as_ref())
            .with_project_relative_path(Some(std::path::Path::new(&file.relative_path)))
            .with_read_tracking_path(Some(&file.path));
            let write = guard.commit_write_content(preflighted, request).await;
            if let Err(error) = write {
                for &(path, relative, content) in rollbacks.iter().rev() {
                    if let Err(rollback_error) = restore_file_content(
                        self.hub.project_root(),
                        path,
                        Some(std::path::Path::new(relative)),
                        content,
                    )
                    .await
                    {
                        tracing::error!(
                            path = %path.display(),
                            %rollback_error,
                            "failed to roll back a rename_symbol edit"
                        );
                    }
                }
                return Err(error);
            }
            rollbacks.push((&file.path, &file.relative_path, &file.old_content));
        }
        for evidence in &preflighted {
            let post = async {
                guard.post_file_write(evidence).await;
                Ok(())
            };
            match workspace_lock.as_ref() {
                Some(workspace_lock) => workspace_lock
                    .run_while_held(post)
                    .await
                    .context(
                        "Symbol rename committed, but PostFileWrite was cancelled after workspace lease loss",
                    )?,
                None => post.await?,
            }
        }
        let summary = rename_summary(&edited, &args.new_name);
        let diff = crate::diff::FileDiff::from_files(
            edited.iter().map(|file| file.diff.clone()).collect(),
        )
        .context("rename_symbol produced no structured file diffs")?;
        let paths = edited
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        self.hub.sync_after_files_changed(&paths).await?;
        Ok(ToolOutput::Edit { summary, diff })
    }
}

fn format_locations(
    project_root: &std::path::Path,
    locations: &[LspLocation],
    empty: &str,
) -> String {
    if locations.is_empty() {
        return empty.to_string();
    }
    locations
        .iter()
        .map(|location| {
            format!(
                "{}:{}:{}",
                lsp::relative_path(project_root, &location.path),
                location.range.start.line + 1,
                location.range.start.character + 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_symbols(project_root: &std::path::Path, symbols: &[LspSymbol]) -> String {
    if symbols.is_empty() {
        return "No workspace symbols found.".to_string();
    }
    symbols
        .iter()
        .map(|symbol| {
            let container = symbol
                .container_name
                .as_ref()
                .map(|name| format!("  {name}"))
                .unwrap_or_default();
            format!(
                "{}  {}  {}:{}:{}{}",
                symbol.kind,
                symbol.name,
                lsp::relative_path(project_root, &symbol.location.path),
                symbol.location.range.start.line + 1,
                symbol.location.range.start.character + 1,
                container,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rename_summary(files: &[crate::lsp::EditedFile], new_name: &str) -> String {
    let file_count = files.len();
    let added: usize = files.iter().map(|file| file.stats().added).sum();
    let removed: usize = files.iter().map(|file| file.stats().removed).sum();
    let mut out = format!(
        "Renamed symbol to '{new_name}' across {file_count} file(s).\nLines added: {added}, Lines removed: {removed}\nFiles:"
    );
    for file in files {
        out.push_str(&format!("\n- {}", file.relative_path));
    }
    if file_count > 1 {
        out.push_str("\nDiff preview shows the first changed file.");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_warmup_classifier_is_narrow() {
        assert!(likely_warming_error(&lsp::LspError::Protocol(
            "request timed out while rust-analyzer indexed".to_string()
        )));
        assert!(likely_warming_error(&lsp::LspError::Protocol(
            "server is not ready".to_string()
        )));
        assert!(!likely_warming_error(&lsp::LspError::Protocol(
            "invalid params: line out of range".to_string()
        )));
    }
}
