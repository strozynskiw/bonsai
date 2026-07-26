//! Multi-file patch tool speaking the `apply_patch` (V4A) format that
//! GPT-family models were trained on in Codex. One call can create, update,
//! move, and delete several files — the fastest way to land a greenfield
//! burst or a cross-file change in a single model round-trip (a Codex run of
//! the same task used ONE 30KB patch where bonsai spent 19 separate
//! mutations).
//!
//! The patch is parsed and applied in memory first; nothing touches disk
//! until every operation has validated and every hunk has matched, so a
//! failing hunk can never leave the tree half-patched.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use strsim::normalized_levenshtein;

use crate::diff::{FileDiff, build_deleted_file_diff, build_file_diff};
use crate::tool::edit_recovery;
use crate::tool::file_mutation::{
    FileMutationAction, FileMutationContext, FileWriteRequest, ParentDirs, ResolvedMutationPath,
    WriteContentContext, WritePrecondition, file_mutation_tool_ctors,
    reject_replayed_elision_marker, remove_created_file, restore_file_content,
};
use crate::tool::schema::{object, parse_args, string_property};
use crate::tool::{
    ActionEffect, ActionPlan, ActionPolicy, MutationTarget, ReadTracker, Tool, ToolOutput,
    WorkspaceLockContext,
};
use crate::yolo::YoloMode;

#[derive(Deserialize)]
struct ApplyPatchArgs {
    #[serde(alias = "patch")]
    input: String,
}

pub struct ApplyPatchTool {
    ctx: FileMutationContext,
}

file_mutation_tool_ctors!(ApplyPatchTool);

// ---------------------------------------------------------------------------
// Patch model + parser
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum PatchOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

impl PatchOp {
    fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }

    /// The path this op ends up writing (post-move destination for a move; the
    /// target for add/update). `None` for a delete — a removed file has no
    /// post-state to diagnose.
    fn written_path(&self) -> Option<&str> {
        match self {
            Self::Add { path, .. } => Some(path),
            Self::Update {
                path,
                move_to: None,
                ..
            } => Some(path),
            Self::Update {
                move_to: Some(dest),
                ..
            } => Some(dest),
            Self::Delete { .. } => None,
        }
    }
}

/// The files a patch will end up writing, given its raw tool arguments (the
/// `input`/`patch` string, or a bare string). Used by the run loop to scope
/// LSP diagnostics to the files an `apply_patch` call actually touched — the
/// analogue of the single `path` arg the write/edit tools expose. A malformed
/// patch yields an empty list; diagnostics are best-effort.
pub(crate) fn patched_paths_from_arguments(arguments: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return Vec::new();
    };
    let input = if value.is_string() {
        value.as_str().map(str::to_string)
    } else {
        value
            .get("input")
            .or_else(|| value.get("patch"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let Some(input) = input else {
        return Vec::new();
    };
    parse_patch(&input)
        .map(|ops| {
            ops.iter()
                .filter_map(|op| op.written_path().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, PartialEq, Default)]
struct Hunk {
    /// Optional `@@ <text>` locator naming a line near the hunk (e.g. a fn
    /// signature). A hint for placement, not a hard requirement.
    locator: Option<String>,
    lines: Vec<HunkLine>,
}

#[derive(Debug, PartialEq)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

const BEGIN_MARKER: &str = "*** Begin Patch";
const END_MARKER: &str = "*** End Patch";
const EOF_MARKER: &str = "*** End of File";
const ADD_DIRECTIVE: &str = "*** Add File: ";
const UPDATE_DIRECTIVE: &str = "*** Update File: ";
const DELETE_DIRECTIVE: &str = "*** Delete File: ";
const MOVE_DIRECTIVE: &str = "*** Move to: ";

fn parse_patch(input: &str) -> Result<Vec<PatchOp>> {
    let mut lines = input.lines().peekable();
    // The envelope is expected but tolerated when missing — a model that
    // sends only the directives should still succeed.
    if lines.peek().map(|l| l.trim()) == Some(BEGIN_MARKER) {
        lines.next();
    }

    let mut ops: Vec<PatchOp> = Vec::new();
    while let Some(&line) = lines.peek() {
        let trimmed = line.trim_end();
        if trimmed.trim() == END_MARKER || trimmed.trim().is_empty() && ops.is_empty() {
            lines.next();
            continue;
        }
        if let Some(path) = trimmed.strip_prefix(ADD_DIRECTIVE) {
            lines.next();
            let mut content_lines: Vec<&str> = Vec::new();
            while let Some(&body) = lines.peek() {
                if body.trim_end().starts_with("*** ") {
                    break;
                }
                lines.next();
                if let Some(rest) = body.strip_prefix('+') {
                    content_lines.push(rest);
                } else if body.is_empty() {
                    // Tolerate a bare empty line where `+` was dropped.
                    content_lines.push("");
                } else {
                    anyhow::bail!(
                        "apply_patch: line inside '*** Add File: {path}' must start with '+' \
                         (got: {body:?}). Every content line of an added file carries a '+' prefix."
                    );
                }
            }
            let mut content = content_lines.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            ops.push(PatchOp::Add {
                path: path.trim().to_string(),
                content,
            });
        } else if let Some(path) = trimmed.strip_prefix(DELETE_DIRECTIVE) {
            lines.next();
            ops.push(PatchOp::Delete {
                path: path.trim().to_string(),
            });
        } else if let Some(path) = trimmed.strip_prefix(UPDATE_DIRECTIVE) {
            lines.next();
            let mut move_to = None;
            if let Some(&next) = lines.peek()
                && let Some(dest) = next.trim_end().strip_prefix(MOVE_DIRECTIVE)
            {
                move_to = Some(dest.trim().to_string());
                lines.next();
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            let mut current = Hunk::default();
            while let Some(&body) = lines.peek() {
                let body_trimmed = body.trim_end();
                if body_trimmed.trim() == EOF_MARKER {
                    lines.next();
                    continue;
                }
                if body_trimmed.starts_with("*** ") {
                    break;
                }
                lines.next();
                if let Some(locator) = body_trimmed.strip_prefix("@@") {
                    if !current.lines.is_empty() {
                        hunks.push(std::mem::take(&mut current));
                    }
                    let locator = locator.trim();
                    current.locator = (!locator.is_empty()).then(|| locator.to_string());
                } else if let Some(rest) = body.strip_prefix('+') {
                    current.lines.push(HunkLine::Add(rest.to_string()));
                } else if let Some(rest) = body.strip_prefix('-') {
                    current.lines.push(HunkLine::Remove(rest.to_string()));
                } else if let Some(rest) = body.strip_prefix(' ') {
                    current.lines.push(HunkLine::Context(rest.to_string()));
                } else {
                    // Tolerate a context line whose leading space was dropped
                    // (common in replayed/reformatted patches).
                    current.lines.push(HunkLine::Context(body.to_string()));
                }
            }
            if !current.lines.is_empty() {
                hunks.push(current);
            }
            if hunks.is_empty() {
                anyhow::bail!(
                    "apply_patch: '*** Update File: {path}' has no hunks. Provide @@ hunks with \
                     context (' '), removed ('-'), and added ('+') lines, or use '*** Add File:' \
                     for a new file."
                );
            }
            ops.push(PatchOp::Update {
                path: path.trim().to_string(),
                move_to,
                hunks,
            });
        } else {
            anyhow::bail!(
                "apply_patch: unrecognized line outside any file section: {trimmed:?}. Expected \
                 '*** Add File: <path>', '*** Update File: <path>', '*** Delete File: <path>', \
                 or '*** End Patch'."
            );
        }
    }

    if ops.is_empty() {
        anyhow::bail!(
            "apply_patch: the patch contains no file operations. Wrap changes in '*** Begin \
             Patch' / '*** End Patch' with '*** Add File:', '*** Update File:', or '*** Delete \
             File:' sections."
        );
    }
    Ok(ops)
}

// ---------------------------------------------------------------------------
// Hunk application
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkMatchKind {
    Exact,
    TrailingWhitespace,
    Indentation,
    FuzzyEdit(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HunkMatch {
    start: usize,
    kind: HunkMatchKind,
}

#[derive(Debug, PartialEq, Eq)]
struct AppliedHunks {
    content: String,
    relaxed_matches: Vec<String>,
}

fn apply_hunks(
    original: &str,
    hunks: &[Hunk],
    path: &str,
    allow_fuzzy_edit_fallback: bool,
) -> Result<AppliedHunks> {
    let had_trailing_newline = original.ends_with('\n') || original.is_empty();
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let mut cursor = 0usize;
    let mut relaxed_matches = Vec::new();

    for (hunk_index, hunk) in hunks.iter().enumerate() {
        if let Some(locator) = &hunk.locator
            && let Some(pos) = lines[cursor..]
                .iter()
                .position(|l| l.trim() == locator.trim())
        {
            cursor += pos;
        }

        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Remove(s) => Some(s.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        if old.is_empty() {
            // Pure addition with no anchor: append at end of file.
            lines.extend(hunk_new_lines(hunk));
            cursor = lines.len();
            continue;
        }

        let matched = find_block(
            &lines,
            cursor,
            &old,
            allow_fuzzy_edit_fallback && hunks.len() == 1,
        )
        .ok_or_else(|| {
            hunk_mismatch_error(&lines, cursor, &old, path, hunk_index.saturating_add(1))
        })?;
        let actual = &lines[matched.start..matched.start + old.len()];
        let new = match matched.kind {
            HunkMatchKind::Exact => hunk_new_lines(hunk),
            HunkMatchKind::TrailingWhitespace
            | HunkMatchKind::Indentation
            | HunkMatchKind::FuzzyEdit(_) => relaxed_hunk_new_lines(hunk, actual, matched.kind),
        };
        if matched.kind != HunkMatchKind::Exact {
            let note = match matched.kind {
                HunkMatchKind::TrailingWhitespace => format!(
                    "hunk {} matched modulo trailing whitespace at lines {}-{}",
                    hunk_index.saturating_add(1),
                    matched.start.saturating_add(1),
                    matched.start.saturating_add(old.len())
                ),
                HunkMatchKind::Indentation => format!(
                    "hunk {} matched modulo indentation at lines {}-{}",
                    hunk_index.saturating_add(1),
                    matched.start.saturating_add(1),
                    matched.start.saturating_add(old.len())
                ),
                HunkMatchKind::FuzzyEdit(similarity) => {
                    format!(
                        "hunk {} used edit-style fuzzy fallback at lines {}-{} ({similarity}% similar)",
                        hunk_index.saturating_add(1),
                        matched.start.saturating_add(1),
                        matched.start.saturating_add(old.len())
                    )
                }
                HunkMatchKind::Exact => unreachable!("filtered above"),
            };
            relaxed_matches.push(note);
        }
        lines.splice(
            matched.start..matched.start + old.len(),
            new.iter().cloned(),
        );
        cursor = matched.start + new.len();
    }

    let mut result = lines.join("\n");
    if had_trailing_newline && !result.is_empty() {
        result.push('\n');
    }
    Ok(AppliedHunks {
        content: result,
        relaxed_matches,
    })
}

fn hunk_new_lines(hunk: &Hunk) -> Vec<String> {
    hunk.lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(text) | HunkLine::Add(text) => Some(text.clone()),
            HunkLine::Remove(_) => None,
        })
        .collect()
}

/// Preserve the file's real context bytes when a relaxed match fires. Added
/// lines inherit an indentation mapping only when the hunk's old lines prove a
/// unique expected-prefix -> actual-prefix relationship.
fn relaxed_hunk_new_lines(hunk: &Hunk, actual: &[String], kind: HunkMatchKind) -> Vec<String> {
    let indentation = if kind == HunkMatchKind::Indentation {
        indentation_mappings(hunk, actual)
    } else {
        Vec::new()
    };
    let mut old_index = 0usize;
    let mut new = Vec::new();
    for line in &hunk.lines {
        match line {
            HunkLine::Context(_) => {
                if let Some(actual_line) = actual.get(old_index) {
                    new.push(actual_line.clone());
                }
                old_index = old_index.saturating_add(1);
            }
            HunkLine::Remove(_) => {
                old_index = old_index.saturating_add(1);
            }
            HunkLine::Add(text) => {
                let expected = leading_whitespace(text);
                let adjusted = indentation
                    .iter()
                    .find(|(from, _to)| *from == expected)
                    .map_or_else(
                        || text.clone(),
                        |(_from, to)| format!("{to}{}", &text[expected.len()..]),
                    );
                new.push(adjusted);
            }
        }
    }
    new
}

fn indentation_mappings<'hunk, 'actual>(
    hunk: &'hunk Hunk,
    actual: &'actual [String],
) -> Vec<(&'hunk str, &'actual str)> {
    let mut mappings = Vec::new();
    let mut ambiguous = Vec::new();
    for (expected, actual) in hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.as_str()),
            HunkLine::Add(_) => None,
        })
        .zip(actual)
    {
        if expected.trim().is_empty() || actual.trim().is_empty() {
            continue;
        }
        let mapping = (leading_whitespace(expected), leading_whitespace(actual));
        if ambiguous.contains(&mapping.0) || mappings.contains(&mapping) {
            continue;
        }
        if let Some(index) = mappings
            .iter()
            .position(|(from, to)| *from == mapping.0 && *to != mapping.1)
        {
            mappings.remove(index);
            ambiguous.push(mapping.0);
        } else {
            mappings.push(mapping);
        }
    }
    mappings
}

fn leading_whitespace(text: &str) -> &str {
    let first_non_whitespace = text
        .char_indices()
        .find_map(|(index, ch)| (!ch.is_whitespace()).then_some(index))
        .unwrap_or(text.len());
    &text[..first_non_whitespace]
}

/// First position at or after `from` where `block` matches `lines`. Exact
/// matching wins, then trailing-whitespace drift, then a unique
/// indentation-only match. The uniqueness requirement is the edit tool's
/// conservative fallback rule: never auto-apply an ambiguous fuzzy guess.
fn find_block(
    lines: &[String],
    from: usize,
    block: &[&str],
    allow_fuzzy_edit_fallback: bool,
) -> Option<HunkMatch> {
    if let Some(start) = window_position(lines, from, block, |a, b| a == b)
        .first()
        .copied()
    {
        return Some(HunkMatch {
            start,
            kind: HunkMatchKind::Exact,
        });
    }
    if let Some(start) = window_position(lines, from, block, |a, b| a.trim_end() == b.trim_end())
        .first()
        .copied()
    {
        return Some(HunkMatch {
            start,
            kind: HunkMatchKind::TrailingWhitespace,
        });
    }
    let indentation = window_position(lines, from, block, |a, b| a.trim() == b.trim());
    if let [start] = indentation.as_slice() {
        return Some(HunkMatch {
            start: *start,
            kind: HunkMatchKind::Indentation,
        });
    }
    if !allow_fuzzy_edit_fallback || lines.len() < block.len() {
        return None;
    }
    let expected = block.join("\n");
    let mut candidates = (0..=lines.len() - block.len())
        .map(|start| {
            let actual = lines[start..start + block.len()].join("\n");
            (start, normalized_levenshtein(&actual, &expected))
        })
        .filter(|(_, score)| *score >= 0.85)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.1.total_cmp(&left.1));
    let (start, score) = *candidates.first()?;
    if candidates
        .get(1)
        .is_some_and(|(_, runner_up)| score - runner_up < 0.05)
    {
        return None;
    }
    Some(HunkMatch {
        start,
        kind: HunkMatchKind::FuzzyEdit((score * 100.0).round() as u8),
    })
}

fn window_position(
    lines: &[String],
    from: usize,
    block: &[&str],
    eq: impl Fn(&str, &str) -> bool,
) -> Vec<usize> {
    if block.is_empty() || lines.len() < block.len() {
        return Vec::new();
    }
    (from..=lines.len() - block.len())
        .filter(|&start| (0..block.len()).all(|i| eq(&lines[start + i], block[i])))
        .collect()
}

fn hunk_mismatch_error(
    lines: &[String],
    expected_start: usize,
    old: &[&str],
    path: &str,
    hunk_number: usize,
) -> anyhow::Error {
    const MAX_ACTUAL_LINES: usize = 12;
    let start = expected_start.min(lines.len());
    let end = start
        .saturating_add(old.len().max(1))
        .min(lines.len())
        .min(start.saturating_add(MAX_ACTUAL_LINES));
    let actual = if start == end {
        "(expected range is past EOF)".to_string()
    } else {
        lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>5}: {line}", start + offset + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let current = lines.join("\n");
    let expected = old.join("\n");
    let nearest = edit_recovery::nearest_match_hint(&current, &expected)
        .map(|hint| format!("\n{hint}"))
        .unwrap_or_default();
    anyhow::anyhow!(
        "apply_patch: hunk {hunk_number} for '{path}' does not match the file. First expected \
         line: {:?}.\nActual file content near the expected range:\n{actual}{nearest}\nRe-read the \
         indicated range and resend the hunk, or use the edit tool with the exact on-disk text.",
        old.first().copied().unwrap_or_default()
    )
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// A fully validated disk action, produced before anything is written.
enum Planned {
    Write {
        resolved: ResolvedMutationPath,
        args_path: String,
        content: String,
        old_content: Option<String>,
        parent_dirs: ParentDirs,
        label: char,
        diff: FileDiff,
    },
    Remove {
        canonical: PathBuf,
        project_relative_path: Option<PathBuf>,
        mutation_target: MutationTarget,
        args_path: String,
        old_content: String,
        diff: FileDiff,
    },
    /// Content already matches — nothing to do, but the op keeps its summary
    /// line and the patch still counts as multi-file for rendering.
    Unchanged,
}

/// The inverse of one committed patch action. Built only after that action
/// succeeds, then replayed in reverse if a later filesystem operation fails so
/// a multi-file patch does not leave a half-applied tree. Hooks are fully
/// preflighted before the first action and post hooks run only after commit.
enum RollbackAction {
    RemoveCreated {
        path: PathBuf,
        project_relative_path: Option<PathBuf>,
    },
    Restore {
        path: PathBuf,
        project_relative_path: Option<PathBuf>,
        content: String,
    },
}

impl Planned {
    fn mutation_target(&self) -> Option<&MutationTarget> {
        match self {
            Self::Write { resolved, .. } => Some(&resolved.mutation_target),
            Self::Remove {
                mutation_target, ..
            } => Some(mutation_target),
            Self::Unchanged => None,
        }
    }

    fn hook_evidence(&self) -> Option<(&str, &FileDiff)> {
        match self {
            Self::Write {
                args_path, diff, ..
            }
            | Self::Remove {
                args_path, diff, ..
            } => Some((args_path, diff)),
            Self::Unchanged => None,
        }
    }
}

async fn rollback(project_root: &Path, actions: &mut Vec<RollbackAction>) {
    while let Some(action) = actions.pop() {
        let result = match action {
            RollbackAction::RemoveCreated {
                path,
                project_relative_path,
            } => remove_created_file(project_root, &path, project_relative_path.as_deref())
                .await
                .with_context(|| format!("failed to remove rolled-back file {}", path.display())),
            RollbackAction::Restore {
                path,
                project_relative_path,
                content,
            } => restore_file_content(
                project_root,
                &path,
                project_relative_path.as_deref(),
                &content,
            )
            .await
            .with_context(|| format!("failed to restore {} during rollback", path.display())),
        };
        if let Err(error) = result {
            tracing::error!(%error, "apply_patch rollback step failed");
        }
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::SelfAuthorized
    }

    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Atomically create or change several already-understood files with a V4A patch. Use \
         '*** Begin Patch', per-file Add/Update/Delete sections with +/-/context lines, then \
         '*** End Patch'. Reserve this for new-file creation or multi-file changes whose existing \
         targets were read in the current turn; use edit for a targeted single-file replacement."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [(
                "input",
                string_property(
                    "The full patch, from '*** Begin Patch' to '*** End Patch' inclusive",
                ),
            )],
            &["input"],
        )
    }

    /// A patch touches an unknowable set of paths in one call, so it must never
    /// run alongside a read or edit of a file it is about to write. Serialize it
    /// (→ GlobalWrite in the batcher) so it always lands in its own batch.
    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        // Some models send the patch as a bare string argument.
        let args = if args.is_string() {
            serde_json::json!({ "input": args })
        } else {
            args
        };
        let args: ApplyPatchArgs = parse_args("apply_patch", args)?;
        let ops = parse_patch(&args.input)?;
        let allow_fuzzy_edit_fallback =
            matches!(ops.as_slice(), [PatchOp::Update { hunks, .. }] if hunks.len() == 1);

        {
            let mut seen = std::collections::HashSet::new();
            for op in &ops {
                if !seen.insert(op.path()) {
                    anyhow::bail!(
                        "apply_patch: '{}' appears in more than one section. Combine the changes \
                         for a file into a single '*** Update File:' section.",
                        op.path()
                    );
                }
            }
        }
        let patch_guard = self.ctx.guard(FileMutationAction::Write);
        let workspace_lock = patch_guard.acquire_write_lock().await?;

        // Phase 1: validate everything and build the full set of disk actions
        // in memory. No writes happen until all operations succeed.
        let mut planned: Vec<Planned> = Vec::new();
        let mut summary_lines: Vec<String> = Vec::new();
        for op in &ops {
            match op {
                PatchOp::Add { path, content } => {
                    reject_replayed_elision_marker(content, path)?;
                    let guard = self.ctx.guard(FileMutationAction::Write);
                    let resolved = guard.resolve_path(path)?;
                    if let Some(canonical) = resolved.canonical_path.as_deref() {
                        guard.ensure_not_directory(canonical, path)?;
                        let existing = tokio::fs::read_to_string(canonical).await.ok();
                        if existing.as_deref() == Some(content.as_str()) {
                            planned.push(Planned::Unchanged);
                            summary_lines
                                .push(format!("= {path} (already contained this content)"));
                            continue;
                        }
                        anyhow::bail!(
                            "apply_patch: '*** Add File: {path}' targets a file that already \
                             exists with different content. Use '*** Update File: {path}' with \
                             hunks (after reading it), or the edit tool."
                        );
                    }
                    let added = content.lines().count();
                    summary_lines.push(format!("A {path} (+{added})"));
                    let diff = build_file_diff(path.clone(), None, content);
                    planned.push(Planned::Write {
                        resolved,
                        args_path: path.clone(),
                        content: content.clone(),
                        old_content: None,
                        parent_dirs: ParentDirs::Create,
                        label: 'A',
                        diff,
                    });
                }
                PatchOp::Delete { path } => {
                    let guard = self.ctx.guard(FileMutationAction::Edit);
                    let resolved = guard.resolve_path(path)?;
                    let Some(canonical) = resolved.canonical_path else {
                        anyhow::bail!("apply_patch: cannot delete '{path}': file not found.");
                    };
                    // Reading first is required — it proves the model has seen
                    // what it is destroying (same rule as editing).
                    let old_content = guard.read_existing_content(&canonical, path).await?;
                    summary_lines.push(format!("D {path}"));
                    let diff = build_deleted_file_diff(path.clone(), &old_content);
                    planned.push(Planned::Remove {
                        canonical,
                        project_relative_path: resolved.project_relative_path,
                        mutation_target: resolved.mutation_target,
                        args_path: path.clone(),
                        old_content,
                        diff,
                    });
                }
                PatchOp::Update {
                    path,
                    move_to,
                    hunks,
                } => {
                    let guard = self.ctx.guard(FileMutationAction::Edit);
                    let resolved = guard.resolve_path(path)?;
                    let Some(canonical) = resolved.canonical_path.clone() else {
                        anyhow::bail!(
                            "apply_patch: cannot update '{path}': file not found. Use '*** Add \
                             File: {path}' to create it."
                        );
                    };
                    let old_content = guard.read_existing_content(&canonical, path).await?;
                    let applied =
                        apply_hunks(&old_content, hunks, path, allow_fuzzy_edit_fallback)?;
                    let new_content = applied.content;
                    if !applied.relaxed_matches.is_empty() {
                        summary_lines
                            .push(format!("~ {path} ({})", applied.relaxed_matches.join("; ")));
                    }

                    if let Some(dest) = move_to {
                        let write_guard = self.ctx.guard(FileMutationAction::Write);
                        let dest_resolved = write_guard.resolve_path(dest)?;
                        if dest_resolved.canonical_path.is_some() {
                            anyhow::bail!(
                                "apply_patch: cannot move '{path}' to '{dest}': the destination \
                                 already exists."
                            );
                        }
                        summary_lines.push(format!("M {path} -> {dest}"));
                        let destination_diff = build_file_diff(dest.clone(), None, &new_content);
                        let source_diff = build_deleted_file_diff(path.clone(), &old_content);
                        planned.push(Planned::Write {
                            resolved: dest_resolved,
                            args_path: dest.clone(),
                            content: new_content,
                            old_content: Some(old_content.clone()),
                            parent_dirs: ParentDirs::Create,
                            label: 'M',
                            diff: destination_diff,
                        });
                        planned.push(Planned::Remove {
                            canonical,
                            project_relative_path: resolved.project_relative_path,
                            mutation_target: resolved.mutation_target,
                            args_path: path.clone(),
                            old_content,
                            diff: source_diff,
                        });
                    } else if new_content == old_content {
                        planned.push(Planned::Unchanged);
                        summary_lines.push(format!("= {path} (already contained this content)"));
                    } else {
                        let added = hunks
                            .iter()
                            .flat_map(|h| &h.lines)
                            .filter(|l| matches!(l, HunkLine::Add(_)))
                            .count();
                        let removed = hunks
                            .iter()
                            .flat_map(|h| &h.lines)
                            .filter(|l| matches!(l, HunkLine::Remove(_)))
                            .count();
                        summary_lines.push(format!("M {path} (+{added} -{removed})"));
                        let diff = build_file_diff(path.clone(), Some(&old_content), &new_content);
                        planned.push(Planned::Write {
                            resolved,
                            args_path: path.clone(),
                            content: new_content,
                            old_content: Some(old_content),
                            parent_dirs: ParentDirs::Keep,
                            label: 'M',
                            diff,
                        });
                    }
                }
            }
        }

        let mut planned_effects = Vec::new();
        for action in &planned {
            match action {
                Planned::Write {
                    content,
                    old_content,
                    ..
                } => {
                    if old_content
                        .as_deref()
                        .is_some_and(|old| !old.is_empty() && content.is_empty())
                    {
                        planned_effects.push(ActionEffect::Truncate);
                    }
                }
                Planned::Remove { .. } => {
                    planned_effects.push(ActionEffect::Delete);
                }
                Planned::Unchanged => {}
            }
        }
        if planned
            .iter()
            .any(|action| action.mutation_target().is_some())
        {
            let plan = ActionPlan::file_mutation(
                "apply_patch",
                planned.iter().filter_map(Planned::mutation_target),
                planned_effects,
            );
            patch_guard.authorize(&plan).await?;
        }

        let structured_diff = FileDiff::from_files(
            planned
                .iter()
                .filter_map(|action| action.hook_evidence().map(|(_, diff)| diff.clone()))
                .collect(),
        );

        let write_guard = self.ctx.guard(FileMutationAction::Write);
        let mut preflighted = Vec::with_capacity(planned.len());
        for (args_path, diff) in planned.iter().filter_map(Planned::hook_evidence) {
            let preflight = write_guard.preflight_file_write("apply_patch", args_path, diff);
            preflighted.push(match workspace_lock.as_ref() {
                Some(workspace_lock) => workspace_lock.run_while_held(preflight).await?,
                None => preflight.await?,
            });
        }

        // Phase 2: every mutation and every veto-capable hook validated —
        // touch the disk. Post hooks wait until the whole transaction commits.
        let mut rollbacks = Vec::new();
        let mut preflighted_iter = preflighted.iter();
        for action in &planned {
            let result = match action {
                Planned::Write {
                    resolved,
                    args_path,
                    content,
                    old_content,
                    parent_dirs,
                    label,
                    diff: _,
                } => {
                    let Some(preflighted) = preflighted_iter.next() else {
                        rollback(self.ctx.project_root(), &mut rollbacks).await;
                        anyhow::bail!("apply_patch lost preflight evidence for {args_path}");
                    };
                    let preexisting = resolved.canonical_path.is_some();
                    let request = FileWriteRequest::new(
                        &resolved.path,
                        content,
                        if resolved.canonical_path.is_some() {
                            WritePrecondition::Exact(old_content.as_deref().unwrap_or_default())
                        } else {
                            WritePrecondition::Absent
                        },
                        if *label == 'A' {
                            WriteContentContext::CreateFile
                        } else {
                            WriteContentContext::WriteFile
                        },
                        *parent_dirs,
                    )
                    .with_workspace_lock(workspace_lock.as_ref())
                    .with_project_relative_path(resolved.project_relative_path.as_deref())
                    .with_read_tracking_path(resolved.canonical_path.as_deref());
                    let write = write_guard.commit_write_content(preflighted, request).await;
                    if write.is_ok() && preexisting {
                        if let Some(old_content) = old_content {
                            rollbacks.push(RollbackAction::Restore {
                                path: resolved.path.clone(),
                                project_relative_path: resolved.project_relative_path.clone(),
                                content: old_content.clone(),
                            });
                        }
                    } else if write.is_ok() {
                        rollbacks.push(RollbackAction::RemoveCreated {
                            path: resolved.path.clone(),
                            project_relative_path: resolved.project_relative_path.clone(),
                        });
                    }
                    write
                }
                Planned::Remove {
                    canonical,
                    project_relative_path,
                    mutation_target: _,
                    args_path,
                    old_content,
                    diff: _,
                } => {
                    let Some(preflighted) = preflighted_iter.next() else {
                        rollback(self.ctx.project_root(), &mut rollbacks).await;
                        anyhow::bail!("apply_patch lost preflight evidence for {args_path}");
                    };
                    let delete = write_guard
                        .commit_delete_file(
                            preflighted,
                            workspace_lock.as_ref(),
                            canonical,
                            project_relative_path.as_deref(),
                            WritePrecondition::Exact(old_content),
                        )
                        .await;
                    if delete.is_ok() {
                        rollbacks.push(RollbackAction::Restore {
                            path: canonical.clone(),
                            project_relative_path: project_relative_path.clone(),
                            content: old_content.clone(),
                        });
                    }
                    delete
                }
                Planned::Unchanged => Ok(()),
            };
            if let Err(error) = result {
                rollback(self.ctx.project_root(), &mut rollbacks).await;
                return Err(error);
            }
        }
        for evidence in &preflighted {
            let post = async {
                write_guard.post_file_write(evidence).await;
                Ok(())
            };
            match workspace_lock.as_ref() {
                Some(workspace_lock) => workspace_lock.run_while_held(post).await.context(
                    "Patch committed, but PostFileWrite was cancelled after workspace lease loss",
                )?,
                None => post.await?,
            }
        }

        if let Some(diff) = structured_diff {
            let stats = diff.stats();
            let heading = if diff.file_count() == 1 {
                format!("Patch applied: {}", diff.path)
            } else {
                "Success. Applied the patch:".to_string()
            };
            return Ok(ToolOutput::Edit {
                summary: format!(
                    "{heading}\n{}\nFiles changed: {}\nLines added: {}, Lines removed: {}",
                    summary_lines.join("\n"),
                    diff.file_count(),
                    stats.added,
                    stats.removed,
                ),
                diff,
            });
        }
        Ok(ToolOutput::Text(format!(
            "Success. Applied the patch:\n{}",
            summary_lines.join("\n")
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{
        Config, ConfigSource, FailureBehavior, HookAction, HookDef, HookEvent, HookMatcher,
    };
    use crate::extension::status::ExtensionRegistry;
    use crate::hooks::HookEngine;
    use crate::interaction::InteractionService;
    use crate::permissions::PermissionManager;
    use crate::tool::test_utils::TestFixture;
    use serde_json::json;

    fn tool(fixture: &TestFixture) -> ApplyPatchTool {
        ApplyPatchTool::new(fixture.project_root.clone(), fixture.read_tracker.clone())
    }

    fn balanced_tool(fixture: &TestFixture) -> ApplyPatchTool {
        let yolo_mode = YoloMode::with_level(crate::tool::ApprovalLevel::Balanced);
        let policy = ActionPolicy::new(
            PermissionManager::memory_only(),
            Arc::new(InteractionService::noninteractive()),
            yolo_mode.clone(),
        );
        ApplyPatchTool::with_hooks_and_locks(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
            Arc::new(HookEngine::disabled()),
            WorkspaceLockContext::disabled(fixture.project_root.clone()),
            policy,
        )
    }

    async fn mark_read(fixture: &TestFixture, path: &std::path::Path) {
        fixture
            .read_tracker
            .mark_read(&path.canonicalize().unwrap())
            .await;
    }

    #[tokio::test]
    async fn later_hook_veto_happens_before_any_patch_write_or_post_event() {
        let fixture = TestFixture::new();
        let hook = HookDef {
            name: "block-second-file".to_string(),
            event: HookEvent::PreFileWrite,
            matcher: HookMatcher::default(),
            action: HookAction::Shell {
                command: "if [ \"$BONSAI_FILE_PATH\" = \"b.txt\" ]; then \
                          if [ -e a.txt ]; then touch .observed-partial-write; fi; \
                          echo blocked >&2; exit 2; fi"
                    .to_string(),
            },
            timeout_secs: 5,
            blocking: true,
            on_failure: FailureBehavior::Block,
            capabilities: Vec::new(),
            enabled: true,
        };
        let post_hook = HookDef {
            name: "record-post-file-write".to_string(),
            event: HookEvent::PostFileWrite,
            matcher: HookMatcher::default(),
            action: HookAction::Shell {
                command: "touch .observed-post-event".to_string(),
            },
            timeout_secs: 5,
            blocking: false,
            on_failure: FailureBehavior::Warn,
            capabilities: Vec::new(),
            enabled: true,
        };
        let config = Config {
            hooks: vec![
                (hook, ConfigSource::Global),
                (post_hook, ConfigSource::Global),
            ],
            ..Config::default()
        };
        let hooks = Arc::new(
            HookEngine::build(
                &config,
                fixture.project_root.clone(),
                PermissionManager::memory_only_hooks(),
                Arc::new(InteractionService::noninteractive()),
                Arc::new(ExtensionRegistry::new()),
                None,
            )
            .await,
        );
        let tool = ApplyPatchTool::with_hooks_and_locks(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            YoloMode::new(),
            hooks,
            WorkspaceLockContext::disabled(fixture.project_root.clone()),
            ActionPolicy::testing(),
        );
        let patch = "*** Begin Patch\n\
                     *** Add File: a.txt\n\
                     +first\n\
                     *** Add File: b.txt\n\
                     +second\n\
                     *** End Patch";

        let error = tool
            .execute(json!({ "input": patch }))
            .await
            .expect_err("the second hook blocks the transaction");
        assert!(error.to_string().contains("blocked"), "{error:#}");
        assert!(
            !fixture.project_root.join("a.txt").exists(),
            "the first write must be rolled back"
        );
        assert!(
            !fixture.project_root.join("b.txt").exists(),
            "the blocked write never reaches disk"
        );
        assert!(
            !fixture
                .project_root
                .join(".observed-partial-write")
                .exists(),
            "all pre hooks must finish before the first disk write"
        );
        assert!(
            !fixture.project_root.join(".observed-post-event").exists(),
            "a vetoed transaction must not emit committed post events"
        );
    }

    #[tokio::test]
    async fn adds_multiple_files_in_one_patch() {
        let fixture = TestFixture::new();
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Add File: Cargo.toml\n\
                     +[package]\n\
                     +name = \"demo\"\n\
                     *** Add File: src/lib.rs\n\
                     +pub fn hello() -> &'static str {\n\
                     +    \"hi\"\n\
                     +}\n\
                     *** End Patch";
        let output = tool.execute(json!({ "input": patch })).await.unwrap();
        let text = output.rendered_summary();
        assert!(text.contains("A Cargo.toml (+2)"), "{text}");
        assert!(text.contains("A src/lib.rs (+3)"), "{text}");
        let ToolOutput::Edit { diff, .. } = &output else {
            panic!("multi-file patch should return one structured edit");
        };
        assert_eq!(diff.file_count(), 2);
        assert_eq!(
            diff.files()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["Cargo.toml", "src/lib.rs"]
        );
        let lib = std::fs::read_to_string(fixture.project_root.join("src/lib.rs")).unwrap();
        assert_eq!(lib, "pub fn hello() -> &'static str {\n    \"hi\"\n}\n");
    }

    #[tokio::test]
    async fn file_hooks_receive_exact_per_file_diffs_for_every_patch_operation() {
        let fixture = TestFixture::new();
        let updated = fixture.create_file("updated.txt", "old value\n");
        let deleted = fixture.create_file("deleted.txt", "delete me\n");
        let moved = fixture.create_file("moved.txt", "move old\n");
        for path in [&updated, &deleted, &moved] {
            mark_read(&fixture, path).await;
        }
        let capture = HookAction::Shell {
            command: "cat >> .file-hook-events.jsonl; printf '\\n' >> .file-hook-events.jsonl"
                .to_string(),
        };
        let hook = |name: &str, event: HookEvent, blocking: bool| HookDef {
            name: name.to_string(),
            event,
            matcher: HookMatcher::default(),
            action: capture.clone(),
            timeout_secs: 5,
            blocking,
            on_failure: FailureBehavior::Block,
            capabilities: Vec::new(),
            enabled: true,
        };
        let hooks = Arc::new(
            HookEngine::build(
                &Config {
                    hooks: vec![
                        (
                            hook("capture-pre", HookEvent::PreFileWrite, true),
                            ConfigSource::Global,
                        ),
                        (
                            hook("capture-post", HookEvent::PostFileWrite, false),
                            ConfigSource::Global,
                        ),
                    ],
                    ..Config::default()
                },
                fixture.project_root.clone(),
                PermissionManager::memory_only_hooks(),
                Arc::new(InteractionService::noninteractive()),
                Arc::new(ExtensionRegistry::new()),
                None,
            )
            .await,
        );
        let tool = ApplyPatchTool::with_hooks_and_locks(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            YoloMode::new(),
            hooks,
            WorkspaceLockContext::disabled(fixture.project_root.clone()),
            ActionPolicy::testing(),
        );
        let patch = "*** Begin Patch\n\
                     *** Add File: added.txt\n\
                     +added\n\
                     *** Update File: updated.txt\n\
                     -old value\n\
                     +new value\n\
                     *** Delete File: deleted.txt\n\
                     *** Update File: moved.txt\n\
                     *** Move to: renamed.txt\n\
                     -move old\n\
                     +move new\n\
                     *** End Patch";

        let output = tool
            .execute(json!({ "input": patch }))
            .await
            .expect("all hook preflights should allow the patch");
        let ToolOutput::Edit { diff, .. } = output else {
            panic!("patch should return structured diffs");
        };
        let events = std::fs::read_to_string(fixture.project_root.join(".file-hook-events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(events.len(), diff.file_count() * 2, "{events:#?}");
        for file_diff in diff.files() {
            let path_events = events
                .iter()
                .filter(|event| event["file_path"] == file_diff.path)
                .collect::<Vec<_>>();
            assert_eq!(path_events.len(), 2, "{}: {path_events:#?}", file_diff.path);
            assert!(
                path_events
                    .iter()
                    .any(|event| event["event"] == "PreFileWrite")
            );
            assert!(
                path_events
                    .iter()
                    .any(|event| event["event"] == "PostFileWrite")
            );
            for event in path_events {
                assert_eq!(event["diff"], file_diff.render_for_hook(), "{event:#?}");
            }
        }

        let event_diff = |path: &str| {
            events
                .iter()
                .find(|event| event["event"] == "PreFileWrite" && event["file_path"] == path)
                .and_then(|event| event["diff"].as_str())
                .unwrap_or_default()
        };
        assert!(event_diff("added.txt").contains("--- /dev/null"));
        assert!(event_diff("updated.txt").contains("-old value"));
        assert!(event_diff("updated.txt").contains("+new value"));
        assert!(event_diff("deleted.txt").contains("+++ /dev/null"));
        assert!(event_diff("renamed.txt").contains("+move new"));
        assert!(event_diff("moved.txt").contains("-move old"));
    }

    #[tokio::test]
    async fn update_hunk_applies_with_context() {
        let fixture = TestFixture::new();
        let file = fixture.create_file("src/main.rs", "fn main() {\n    old();\n}\n");
        mark_read(&fixture, &file).await;
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: src/main.rs\n\
                     @@ fn main() {\n\
                     -    old();\n\
                     +    new();\n\
                     *** End Patch";
        let output = tool.execute(json!({ "input": patch })).await.unwrap();
        assert!(
            output.rendered_summary().contains("Patch applied"),
            "{}",
            output.rendered_summary()
        );
        assert!(
            matches!(output, ToolOutput::Edit { .. }),
            "single-file update should render a diff card"
        );
        let content = std::fs::read_to_string(file).unwrap();
        assert_eq!(content, "fn main() {\n    new();\n}\n");
    }

    #[tokio::test]
    async fn update_requires_prior_read() {
        let fixture = TestFixture::new();
        fixture.create_file("src/main.rs", "fn main() {}\n");
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: src/main.rs\n\
                     -fn main() {}\n\
                     +fn main() { run(); }\n\
                     *** End Patch";
        let err = tool
            .execute(json!({ "input": patch }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be read before"), "{err}");
    }

    #[tokio::test]
    async fn mismatched_hunk_leaves_all_files_untouched() {
        let fixture = TestFixture::new();
        let ok = fixture.create_file("a.txt", "alpha\n");
        mark_read(&fixture, &ok).await;
        let bad = fixture.create_file("b.txt", "content that is almost there\n");
        mark_read(&fixture, &bad).await;
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: a.txt\n\
                     -alpha\n\
                     +beta\n\
                     *** Update File: b.txt\n\
                     -content that is not there\n\
                     +replacement\n\
                     *** End Patch";
        let err = tool
            .execute(json!({ "input": patch }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match the file"), "{err}");
        assert!(
            err.contains("Actual file content near the expected range"),
            "{err}"
        );
        assert!(err.contains("Closest match"), "{err}");
        // Atomicity: the matching first hunk must not have been written.
        assert_eq!(std::fs::read_to_string(ok).unwrap(), "alpha\n");
        assert_eq!(
            std::fs::read_to_string(bad).unwrap(),
            "content that is almost there\n"
        );
    }

    #[tokio::test]
    async fn unique_indentation_drift_uses_edit_style_fallback_and_reports_it() {
        let fixture = TestFixture::new();
        let file = fixture.create_file(
            "src/main.rs",
            "fn main() {\n        old();\n        keep();\n}\n",
        );
        mark_read(&fixture, &file).await;
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: src/main.rs\n\
                     -    old();\n\
                     +    new();\n\
                          keep();\n\
                     *** End Patch";

        let output = tool.execute(json!({ "input": patch })).await.unwrap();

        assert!(
            output
                .rendered_summary()
                .contains("matched modulo indentation"),
            "{}",
            output.rendered_summary()
        );
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "fn main() {\n        new();\n        keep();\n}\n"
        );
    }

    #[tokio::test]
    async fn single_file_patch_uses_unique_fuzzy_edit_fallback() {
        let fixture = TestFixture::new();
        let file = fixture.create_file(
            "src/lib.rs",
            "fn load() {\n    let value = service.fetch_one(&self.pool)?;\n    save(value);\n}\n",
        );
        mark_read(&fixture, &file).await;
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: src/lib.rs\n\
                     -    let value = service.fetch_one(&pool)?;\n\
                     -    save(value);\n\
                     +    let value = service.fetch_optional(&self.pool)?;\n\
                     +    save(value);\n\
                     *** End Patch";

        let output = tool.execute(json!({ "input": patch })).await.unwrap();

        assert!(
            output
                .rendered_summary()
                .contains("edit-style fuzzy fallback"),
            "{}",
            output.rendered_summary()
        );
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "fn load() {\n    let value = service.fetch_optional(&self.pool)?;\n    save(value);\n}\n"
        );
    }

    #[test]
    fn ambiguous_indentation_drift_is_not_auto_applied() {
        let lines = vec![
            "    old();".to_string(),
            "    keep();".to_string(),
            "        old();".to_string(),
            "        keep();".to_string(),
        ];
        let block = ["old();", "keep();"];

        assert_eq!(find_block(&lines, 0, &block, false), None);
    }

    #[tokio::test]
    async fn delete_and_move_work_after_read() {
        let fixture = TestFixture::new();
        let gone = fixture.create_file("old.txt", "bye\n");
        mark_read(&fixture, &gone).await;
        let moved = fixture.create_file("src/util.rs", "pub fn util() {}\n");
        mark_read(&fixture, &moved).await;
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Delete File: old.txt\n\
                     *** Update File: src/util.rs\n\
                     *** Move to: src/helpers.rs\n\
                     -pub fn util() {}\n\
                     +pub fn helpers() {}\n\
                     *** End Patch";
        let output = tool.execute(json!({ "input": patch })).await.unwrap();
        let text = output.rendered_summary();
        assert!(text.contains("D old.txt"), "{text}");
        assert!(text.contains("M src/util.rs -> src/helpers.rs"), "{text}");
        assert!(!gone.exists());
        assert!(!moved.exists());
        let content = std::fs::read_to_string(fixture.project_root.join("src/helpers.rs")).unwrap();
        assert_eq!(content, "pub fn helpers() {}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn update_alias_to_protected_target_requires_permission() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new();
        let target = fixture.create_file(".env", "TOKEN=old\n");
        mark_read(&fixture, &target).await;
        symlink(".env", fixture.project_root.join("settings.txt")).unwrap();
        let tool = balanced_tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: settings.txt\n\
                     -TOKEN=old\n\
                     +TOKEN=new\n\
                     *** End Patch";

        let error = tool
            .execute(json!({ "input": patch }))
            .await
            .expect_err("patching a protected resolved target must require permission");

        assert!(
            error.to_string().contains("Permission prompt unavailable"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "TOKEN=old\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn move_from_protected_alias_requires_permission() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new();
        let target = fixture.create_file(".env", "TOKEN=old\n");
        mark_read(&fixture, &target).await;
        symlink(".env", fixture.project_root.join("settings.txt")).unwrap();
        let tool = balanced_tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: settings.txt\n\
                     *** Move to: moved.txt\n\
                     -TOKEN=old\n\
                     +TOKEN=new\n\
                     *** End Patch";

        let error = tool
            .execute(json!({ "input": patch }))
            .await
            .expect_err("moving from a protected resolved target must require permission");

        assert!(
            error.to_string().contains("Permission prompt unavailable"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "TOKEN=old\n");
        assert!(!fixture.project_root.join("moved.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn move_into_symlinked_protected_directory_requires_permission() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new();
        let source = fixture.create_file("source.txt", "old\n");
        mark_read(&fixture, &source).await;
        std::fs::create_dir(fixture.project_root.join(".ssh")).unwrap();
        symlink(".ssh", fixture.project_root.join("safe-dir")).unwrap();
        let tool = balanced_tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: source.txt\n\
                     *** Move to: safe-dir/config\n\
                     -old\n\
                     +new\n\
                     *** End Patch";

        let error = tool
            .execute(json!({ "input": patch }))
            .await
            .expect_err("moving into a protected resolved target must require permission");

        assert!(
            error.to_string().contains("Permission prompt unavailable"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(source).unwrap(), "old\n");
        assert!(!fixture.project_root.join(".ssh/config").exists());
    }

    #[tokio::test]
    async fn add_of_identical_existing_file_is_a_noop_success() {
        let fixture = TestFixture::new();
        fixture.create_file("same.txt", "content\n");
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Add File: same.txt\n\
                     +content\n\
                     *** End Patch";
        let output = tool.execute(json!({ "input": patch })).await.unwrap();
        assert!(
            output
                .rendered_summary()
                .contains("already contained this content"),
            "{}",
            output.rendered_summary()
        );
    }

    #[tokio::test]
    async fn add_over_different_existing_file_is_rejected() {
        let fixture = TestFixture::new();
        fixture.create_file("taken.txt", "original\n");
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Add File: taken.txt\n\
                     +other\n\
                     *** End Patch";
        let err = tool
            .execute(json!({ "input": patch }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(
            std::fs::read_to_string(fixture.project_root.join("taken.txt")).unwrap(),
            "original\n"
        );
    }

    #[tokio::test]
    async fn bare_string_args_and_missing_envelope_are_tolerated() {
        let fixture = TestFixture::new();
        let tool = tool(&fixture);
        let patch = "*** Add File: note.md\n+hello";
        let output = tool.execute(json!(patch)).await.unwrap();
        // A single-file patch takes the diff-card path, not the A/M/D summary.
        assert!(
            output.rendered_summary().contains("Patch applied: note.md"),
            "{}",
            output.rendered_summary()
        );
        assert_eq!(
            std::fs::read_to_string(fixture.project_root.join("note.md")).unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn elision_marker_as_add_content_is_rejected() {
        let fixture = TestFixture::new();
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Add File: doc.md\n\
                     +<content elided: 9187 bytes, 273 lines>\n\
                     *** End Patch";
        let err = tool
            .execute(json!({ "input": patch }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("context-elision placeholder"), "{err}");
    }

    #[tokio::test]
    async fn duplicate_paths_in_one_patch_are_rejected() {
        let fixture = TestFixture::new();
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Add File: x.txt\n\
                     +one\n\
                     *** Add File: x.txt\n\
                     +two\n\
                     *** End Patch";
        let err = tool
            .execute(json!({ "input": patch }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than one section"), "{err}");
    }

    #[tokio::test]
    async fn empty_patch_is_rejected_with_guidance() {
        let fixture = TestFixture::new();
        let tool = tool(&fixture);
        let err = tool
            .execute(json!({ "input": "*** Begin Patch\n*** End Patch" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no file operations"), "{err}");
    }

    #[tokio::test]
    async fn eof_marker_and_patch_alias_are_accepted() {
        let fixture = TestFixture::new();
        let file = fixture.create_file("tail.txt", "start\nend\n");
        mark_read(&fixture, &file).await;
        let tool = tool(&fixture);
        let patch = "*** Begin Patch\n\
                     *** Update File: tail.txt\n\
                     @@\n\
                      end\n\
                     +appended\n\
                     *** End of File\n\
                     *** End Patch";
        tool.execute(json!({ "patch": patch })).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "start\nend\nappended\n"
        );
    }
}
