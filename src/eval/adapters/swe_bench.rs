use std::fs;
use std::path::{Component, Path};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Official SWE-bench prediction record consumed by the pinned harnesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SweBenchPrediction {
    pub(crate) instance_id: String,
    pub(crate) model_patch: String,
    pub(crate) model_name_or_path: String,
}

/// Safe text patch plus deterministic evidence recorded outside the prediction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtractedPatch {
    pub(crate) body: String,
    pub(crate) digest: String,
    pub(crate) bytes: usize,
}

/// Extract the complete tracked and untracked text diff against the supplied
/// SWE-bench base commit.
///
/// # Errors
///
/// Returns an error when the workspace is not a Git root, the base is invalid,
/// Git fails, or the result contains binary, symlinked, non-UTF-8, or oversized
/// untracked content.
pub(crate) fn extract_patch(
    workspace: &Path,
    base_commit: &str,
    max_bytes: usize,
) -> Result<ExtractedPatch> {
    let workspace = workspace.canonicalize().with_context(|| {
        format!(
            "Failed to resolve SWE-bench workspace {}",
            workspace.display()
        )
    })?;
    ensure_git_root(&workspace)?;
    verify_base_commit(&workspace, base_commit)?;

    let tracked_path = tempfile::NamedTempFile::new()
        .context("Failed to create temporary tracked-patch file")?
        .into_temp_path();
    let output_arg = format!("--output={}", tracked_path.display());
    git_output_bytes(
        &workspace,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "--no-ext-diff",
            "--full-index",
            "--no-color",
            &output_arg,
            base_commit,
            "--",
        ],
        &[0],
    )?;
    let tracked_len = fs::metadata(&tracked_path)
        .context("Failed to inspect temporary tracked patch")?
        .len();
    if tracked_len > max_bytes as u64 {
        anyhow::bail!(
            "SWE-bench patch is at least {tracked_len} bytes; configured maximum is {max_bytes}"
        );
    }
    let tracked =
        fs::read_to_string(&tracked_path).context("Git emitted non-UTF-8 tracked patch text")?;
    reject_binary_patch(&tracked)?;
    let mut patch = tracked;

    let untracked = git_output_bytes(
        &workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &[0],
    )?;
    for raw_path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = std::str::from_utf8(raw_path)
            .context("SWE-bench workspace contains a non-UTF-8 untracked path")?;
        validate_git_relative_path(relative)?;
        let path = workspace.join(relative);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Failed to inspect untracked file {relative}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!("SWE-bench patch rejects non-regular untracked path: {relative}");
        }
        let remaining = max_bytes.saturating_sub(patch.len());
        if metadata.len() > remaining as u64 {
            anyhow::bail!("SWE-bench patch exceeds the configured maximum of {max_bytes} bytes");
        }
        let bytes =
            fs::read(&path).with_context(|| format!("Failed to read untracked file {relative}"))?;
        if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
            anyhow::bail!("SWE-bench patch rejects binary untracked file: {relative}");
        }
        let fragment = git_output_bytes(
            &workspace,
            &[
                "diff",
                "--no-index",
                "--no-color",
                "--",
                "/dev/null",
                relative,
            ],
            &[0, 1],
        )?;
        let fragment = String::from_utf8(fragment)
            .with_context(|| format!("Git emitted non-UTF-8 patch text for {relative}"))?;
        reject_binary_patch(&fragment)?;
        patch.push_str(&fragment);
        if patch.len() > max_bytes {
            anyhow::bail!(
                "SWE-bench patch is {} bytes; configured maximum is {}",
                patch.len(),
                max_bytes
            );
        }
    }

    if patch.len() > max_bytes {
        anyhow::bail!(
            "SWE-bench patch is {} bytes; configured maximum is {}",
            patch.len(),
            max_bytes
        );
    }
    Ok(ExtractedPatch {
        digest: blake3::hash(patch.as_bytes()).to_hex().to_string(),
        bytes: patch.len(),
        body: patch,
    })
}

fn ensure_git_root(workspace: &Path) -> Result<()> {
    let root = git_output(workspace, &["rev-parse", "--show-toplevel"])?;
    let root = Path::new(root.trim())
        .canonicalize()
        .context("Failed to resolve Git top-level directory")?;
    if root != workspace {
        anyhow::bail!(
            "SWE-bench workspace {} is not the Git repository root {}",
            workspace.display(),
            root.display()
        );
    }
    Ok(())
}

fn verify_base_commit(workspace: &Path, base_commit: &str) -> Result<()> {
    if !(7..=64).contains(&base_commit.len())
        || !base_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("SWE-bench base commit must be a 7-64 character hexadecimal object id");
    }
    let revision = format!("{base_commit}^{{commit}}");
    git_output(workspace, &["rev-parse", "--verify", &revision]).map(|_| ())
}

fn validate_git_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("Git returned an unsafe untracked path: {}", path.display());
    }
    Ok(())
}

fn reject_binary_patch(patch: &str) -> Result<()> {
    if patch.contains("GIT binary patch") || patch.contains("Binary files ") {
        anyhow::bail!("SWE-bench patch contains binary content");
    }
    Ok(())
}

fn git_output(workspace: &Path, args: &[&str]) -> Result<String> {
    let bytes = git_output_bytes(workspace, args, &[0])?;
    String::from_utf8(bytes).context("Git emitted non-UTF-8 output")
}

fn git_output_bytes(workspace: &Path, args: &[&str], accepted: &[i32]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;
    let code = output.status.code().unwrap_or(-1);
    if !accepted.contains(&code) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed ({code}): {}", args.join(" "), stderr.trim());
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(workspace: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn repository() -> (tempfile::TempDir, String) {
        let temp = tempfile::TempDir::new().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "eval@example.test"]);
        git(temp.path(), &["config", "user.name", "Eval"]);
        fs::write(temp.path().join("tracked.txt"), "before\n").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "base"]);
        let base = git(temp.path(), &["rev-parse", "HEAD"]);
        (temp, base)
    }

    #[test]
    fn patch_includes_tracked_and_untracked_text() {
        let (temp, base) = repository();
        fs::write(temp.path().join("tracked.txt"), "after\n").unwrap();
        fs::write(temp.path().join("new file.txt"), "new\n").unwrap();

        let patch = extract_patch(temp.path(), &base, 100_000).unwrap();

        assert!(patch.body.contains("tracked.txt"));
        assert!(patch.body.contains("new file.txt"));
        assert!(patch.body.contains("+after"));
        assert!(patch.body.contains("+new"));
        assert_eq!(patch.bytes, patch.body.len());
    }

    #[test]
    fn patch_rejects_binary_and_oversized_content() {
        let (temp, base) = repository();
        fs::write(temp.path().join("binary.bin"), [0, 1, 2]).unwrap();
        let error = extract_patch(temp.path(), &base, 100_000)
            .unwrap_err()
            .to_string();
        assert!(error.contains("binary"), "{error}");

        fs::remove_file(temp.path().join("binary.bin")).unwrap();
        fs::write(temp.path().join("large.txt"), "x".repeat(1_000)).unwrap();
        let error = extract_patch(temp.path(), &base, 100)
            .unwrap_err()
            .to_string();
        assert!(error.contains("configured maximum"), "{error}");
    }

    #[test]
    fn empty_patch_is_valid_and_stable() {
        let (temp, base) = repository();
        let first = extract_patch(temp.path(), &base, 100_000).unwrap();
        let second = extract_patch(temp.path(), &base, 100_000).unwrap();
        assert!(first.body.is_empty());
        assert_eq!(first.digest, second.digest);
    }
}
