use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::tool::is_safe_relative_path;

const MAX_DETAIL_CHARS: usize = 4_000;

/// A relative path that has been validated as safe (non-empty, no `..`, not
/// absolute), tagged with a human-readable `label` for error messages.
///
/// Replaces the previous `safe_join` / `validate_safe_relative` helper pair: a
/// value is validated exactly once via [`SafeRelativePath::parse`] and then
/// joined onto any base directory with [`SafeRelativePath::join`].
#[derive(Debug, Clone)]
pub(crate) struct SafeRelativePath {
    value: String,
}

impl SafeRelativePath {
    /// Validate `value` as a safe relative path, using `label` to describe it in
    /// any error.
    ///
    /// # Errors
    /// Returns an error if `value` is blank or is not a safe relative path.
    pub(crate) fn parse(value: &str, label: &'static str) -> Result<Self> {
        if value.trim().is_empty() {
            anyhow::bail!("{label} is required");
        }
        if !is_safe_relative_path(Path::new(value)) {
            anyhow::bail!("{label} must be a safe relative path: {value}");
        }
        Ok(Self {
            value: value.to_string(),
        })
    }

    /// Join the validated relative path onto `base`.
    pub(crate) fn join(&self, base: &Path) -> PathBuf {
        base.join(&self.value)
    }
}

/// Recursively copy a fixture directory tree, rejecting symlinks and other
/// non-regular entries so eval worktrees stay self-contained.
///
/// # Errors
/// Returns an error if any directory cannot be created/read, a file cannot be
/// copied, or the source contains a symlink or other unsupported entry.
pub(crate) fn copy_dir_all(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("Failed to create worktree directory {:?}", destination))?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("Failed to read fixture directory {:?}", source))?
    {
        let entry = entry?;
        let ty = entry.file_type()?;
        let source_path = entry.path();
        let dest_path = destination.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&source_path, &dest_path)?;
        } else if ty.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "Failed to copy fixture file {:?} to {:?}",
                    source_path, dest_path
                )
            })?;
        } else if ty.is_symlink() {
            anyhow::bail!(
                "Fixture contains unsupported symlink: {}",
                source_path.display()
            );
        } else {
            anyhow::bail!(
                "Fixture contains unsupported file type: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

/// Truncate a grader detail string to [`MAX_DETAIL_CHARS`] characters, appending
/// a `[truncated]` marker when clipped.
pub(crate) fn truncate_detail(text: &str) -> String {
    if text.chars().count() <= MAX_DETAIL_CHARS {
        return text.to_string();
    }
    let mut truncated = text.chars().take(MAX_DETAIL_CHARS).collect::<String>();
    truncated.push_str("\n[truncated]");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_relative_paths_are_rejected() {
        assert!(SafeRelativePath::parse("src/lib.rs", "path").is_ok());
        assert!(SafeRelativePath::parse("../src/lib.rs", "path").is_err());
        assert!(SafeRelativePath::parse("/tmp/file", "path").is_err());
        assert!(SafeRelativePath::parse("", "path").is_err());
    }
}
