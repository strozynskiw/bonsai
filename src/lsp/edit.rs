use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::diff::{DiffStats, FileDiff, build_file_diff};
use crate::tool::ReadTracker;

use super::protocol::{self, LspTextEdit, LspWorkspaceEdit, uri_to_path};
use super::{LspError, relative_path};

/// One file planned by a workspace edit, with the exact before/after images
/// needed for a guarded commit and potential rollback.
#[derive(Debug, Clone)]
pub(crate) struct EditedFile {
    pub(crate) path: PathBuf,
    pub(crate) relative_path: String,
    pub(crate) old_content: String,
    pub(crate) new_content: String,
    pub(crate) diff: FileDiff,
}

impl EditedFile {
    pub(crate) fn stats(&self) -> DiffStats {
        self.diff.stats()
    }
}

/// Build every in-project text replacement without touching the filesystem.
///
/// The caller owns authorization and commit. Keeping this module side-effect
/// free lets `rename_symbol` reuse the file-mutation guard for lifecycle hooks,
/// CAS writes, workspace locking, and rollback across the complete plan.
pub(crate) async fn plan_workspace_edit(
    project_root: &Path,
    read_tracker: &ReadTracker,
    edit: &LspWorkspaceEdit,
) -> Result<Vec<EditedFile>> {
    let canonical_root = project_root
        .canonicalize()
        .context("failed to canonicalize project root")?;
    let grouped = grouped_project_edits(&canonical_root, edit)?;
    let unread = unread_or_stale_paths(read_tracker, &grouped).await;
    if !unread.is_empty() {
        anyhow::bail!(
            "rename_symbol would edit unread or stale files. Read these files first, then retry:\n{}",
            unread
                .iter()
                .map(|path| format!("- {}", relative_path(&canonical_root, path)))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    let mut edited = Vec::new();
    for (path, edits) in grouped {
        let old_content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let new_content = apply_text_edits(&old_content, &edits)
            .map_err(|err| anyhow::anyhow!("failed to apply edits to {}: {err}", path.display()))?;
        if old_content == new_content {
            continue;
        }
        let relative = relative_path(&canonical_root, &path);
        let diff = build_file_diff(relative.clone(), Some(&old_content), &new_content);
        edited.push(EditedFile {
            path,
            relative_path: relative,
            old_content,
            new_content,
            diff,
        });
    }
    Ok(edited)
}

fn grouped_project_edits(
    canonical_root: &Path,
    edit: &LspWorkspaceEdit,
) -> Result<BTreeMap<PathBuf, Vec<LspTextEdit>>> {
    let mut grouped = BTreeMap::new();
    for (uri, edits) in &edit.changes {
        let path = uri_to_path(uri)?;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize edit path {}", path.display()))?;
        if !canonical.starts_with(canonical_root) {
            anyhow::bail!(
                "language server returned an out-of-project edit: {}",
                canonical.display()
            );
        }
        grouped
            .entry(canonical)
            .or_insert_with(Vec::new)
            .extend(edits.clone());
    }
    Ok(grouped)
}

async fn unread_or_stale_paths(
    read_tracker: &ReadTracker,
    grouped: &BTreeMap<PathBuf, Vec<LspTextEdit>>,
) -> Vec<PathBuf> {
    let mut unread = Vec::new();
    for path in grouped.keys() {
        if !read_tracker.is_read(path).await || !read_tracker.is_unchanged_since_read(path).await {
            unread.push(path.clone());
        }
    }
    unread
}

fn apply_text_edits(content: &str, edits: &[LspTextEdit]) -> Result<String, LspError> {
    let mut ranges = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = protocol::offset_at_position(content, edit.range.start)?;
        let end = protocol::offset_at_position(content, edit.range.end)?;
        if start > end {
            return Err(LspError::Protocol(
                "text edit range starts after it ends".into(),
            ));
        }
        ranges.push((start, end, edit.new_text.clone()));
    }
    ranges.sort_by_key(|(start, end, _)| (*start, *end));
    let mut previous_end = 0usize;
    for (index, (start, end, _)) in ranges.iter().enumerate() {
        if index > 0 && *start < previous_end {
            return Err(LspError::Protocol(
                "language server returned overlapping text edits".into(),
            ));
        }
        previous_end = *end;
    }

    let mut output = content.to_string();
    for (start, end, new_text) in ranges.into_iter().rev() {
        output.replace_range(start..end, &new_text);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::protocol::{LspPosition, LspRange, path_to_file_uri};
    use crate::tool::ReadTracker;

    #[test]
    fn applies_edits_from_bottom_to_top() {
        let edits = vec![
            LspTextEdit {
                range: LspRange {
                    start: LspPosition {
                        line: 0,
                        character: 0,
                    },
                    end: LspPosition {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "one".to_string(),
            },
            LspTextEdit {
                range: LspRange {
                    start: LspPosition {
                        line: 1,
                        character: 0,
                    },
                    end: LspPosition {
                        line: 1,
                        character: 3,
                    },
                },
                new_text: "two".to_string(),
            },
        ];
        assert_eq!(
            apply_text_edits("abc\ndef\n", &edits).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn rejects_overlapping_edits() {
        let edits = vec![
            LspTextEdit {
                range: LspRange {
                    start: LspPosition {
                        line: 0,
                        character: 0,
                    },
                    end: LspPosition {
                        line: 0,
                        character: 2,
                    },
                },
                new_text: "x".to_string(),
            },
            LspTextEdit {
                range: LspRange {
                    start: LspPosition {
                        line: 0,
                        character: 1,
                    },
                    end: LspPosition {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "y".to_string(),
            },
        ];
        assert!(matches!(
            apply_text_edits("abcd", &edits),
            Err(LspError::Protocol(message)) if message == "language server returned overlapping text edits"
        ));
    }

    #[tokio::test]
    async fn plans_workspace_edits_without_writing_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("source.rs");
        tokio::fs::write(&file, "old\n").await.unwrap();
        let canonical = file.canonicalize().unwrap();
        let tracker = ReadTracker::new();
        tracker
            .mark_read_with_content(&canonical, b"old\n", true)
            .await;

        let mut changes = BTreeMap::new();
        changes.insert(
            path_to_file_uri(&canonical).unwrap(),
            vec![LspTextEdit {
                range: LspRange {
                    start: LspPosition {
                        line: 0,
                        character: 0,
                    },
                    end: LspPosition {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "new".to_string(),
            }],
        );

        let planned = plan_workspace_edit(dir.path(), &tracker, &LspWorkspaceEdit { changes })
            .await
            .unwrap();

        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].old_content, "old\n");
        assert_eq!(planned[0].new_content, "new\n");
        assert_eq!(
            tokio::fs::read_to_string(&canonical).await.unwrap(),
            "old\n"
        );
    }
}
