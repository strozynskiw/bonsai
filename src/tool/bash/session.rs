//! Shell session state: the persistent working directory tracking and the
//! session-scoped sandbox-escape grants that outlive a single command.
//!
//! The cwd helpers live as `impl BashTool` methods here rather than as a
//! standalone type — they read the tool's confinement level and canonical roots
//! directly — but they are the only place the tracked `cwd` is advanced or
//! clamped, keeping that invariant in one file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::BashTool;
use super::command::{extract_cd_target, leading_cd_and_remainder};
use crate::tool::is_safe_relative_path;

/// Outcome of the sandbox-escape gate for one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EscapeApproval {
    /// Escape not requested, or the sandbox was inactive — ran normally.
    None,
    /// Approved for this one run.
    Once,
    /// Approved and remembered for the rest of the session.
    Session,
    /// A prior session grant matched — ran unconfined without re-prompting.
    Cached,
    /// An exact, previously denied confined run was deterministically judged
    /// safe enough for the active auto-accepting autonomy level.
    Automatic,
}

impl EscapeApproval {
    /// Did the command actually step outside the sandbox?
    pub(super) fn escaped(self) -> bool {
        !matches!(self, EscapeApproval::None)
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            EscapeApproval::None => "none",
            EscapeApproval::Once => "once",
            EscapeApproval::Session => "for session",
            EscapeApproval::Cached => "session (cached)",
            EscapeApproval::Automatic => "automatic safe retry",
        }
    }
}

/// In-memory, session-scoped record of `(working-dir, command)` pairs the user
/// approved to run outside the sandbox "for the session". Keyed by the **exact**
/// command string *and* its resolved cwd — an escape steps past the enforcement
/// floor, so one approval must neither generalize to a command family nor, at an
/// unclamped autonomy level where the model can `cd` anywhere, silently apply in
/// a different directory than the one the user saw. Normal and SMOL bash tools
/// share this state so toggling output/profile budgets does not reset session
/// grants.
#[derive(Default)]
pub(super) struct SandboxEscapeGrants {
    inner: std::sync::Mutex<std::collections::HashSet<(PathBuf, String)>>,
}

impl SandboxEscapeGrants {
    pub(super) fn is_granted(&self, cwd: &Path, command: &str) -> bool {
        self.inner
            .lock()
            .map(|grants| grants.contains(&(cwd.to_path_buf(), command.to_string())))
            .unwrap_or(false)
    }

    pub(super) fn grant(&self, cwd: &Path, command: &str) {
        if let Ok(mut grants) = self.inner.lock() {
            grants.insert((cwd.to_path_buf(), command.to_string()));
        }
    }
}

/// Session-scoped record of `(working-dir, command)` pairs whose *confined* run
/// failed with a sandbox-shaped denial in the output. Consulted by the escape
/// gate so the "decline unnecessary escape" shortcut for workspace-only git
/// commands can never trap the model in a loop: the failure diagnostic tells it
/// to retry with `escape_sandbox=true`, so silently declining that retry would
/// re-run confined, fail identically, and repeat (observed live with a
/// pre-commit hook whose `mktemp` the sandbox denied). One sandbox-shaped
/// failure lifts the shortcut for that exact command, and the next escape
/// request reaches the normal user approval prompt. Keyed like
/// [`SandboxEscapeGrants`]: exact command string plus resolved cwd.
#[derive(Default)]
pub(super) struct ConfinedFailures {
    inner: std::sync::Mutex<std::collections::HashSet<(PathBuf, String)>>,
}

impl ConfinedFailures {
    pub(super) fn contains(&self, cwd: &Path, command: &str) -> bool {
        self.inner
            .lock()
            .map(|failures| failures.contains(&(cwd.to_path_buf(), command.to_string())))
            .unwrap_or(false)
    }

    pub(super) fn record(&self, cwd: &Path, command: &str) {
        if let Ok(mut failures) = self.inner.lock() {
            failures.insert((cwd.to_path_buf(), command.to_string()));
        }
    }
}

impl BashTool {
    /// Remove a simple leading `cd` only when it resolves to `cwd` exactly.
    ///
    /// The normalized command is used by the authorization gate, hooks, and
    /// executor. This preserves the intended safety classification for routine
    /// commands such as `cargo fmt` while leaving every meaningful directory
    /// change intact.
    pub(super) async fn normalize_redundant_leading_cd(&self, command: &str, cwd: &Path) -> String {
        let Some((target, remainder)) = leading_cd_and_remainder(command) else {
            return command.to_string();
        };
        let target_path = Path::new(&target);
        let candidate = if target_path.is_absolute() {
            target_path.to_path_buf()
        } else {
            cwd.join(target_path)
        };
        let Ok(canonical_target) = tokio::fs::canonicalize(candidate).await else {
            return command.to_string();
        };
        if canonical_target != cwd {
            return command.to_string();
        }

        tracing::debug!(cwd = %cwd.display(), "ignored redundant leading bash cd");
        remainder.to_string()
    }

    /// Resolve the directory a command should run in. Confined runs are clamped
    /// to the project root; an explicit `workdir` outside it is rejected.
    pub(super) async fn resolve_workdir(&self, workdir: Option<&str>) -> Result<PathBuf> {
        let confined = self.yolo_mode.level().is_confined();
        let current_cwd = {
            let mut cwd = self.cwd.lock().await;
            if confined && !cwd.starts_with(&self.canonical_project_root) {
                *cwd = self.canonical_project_root.clone();
            }
            cwd.clone()
        };

        let base = match workdir {
            Some(wd) => {
                let path = if std::path::Path::new(wd).is_absolute() {
                    std::path::PathBuf::from(wd)
                } else if confined {
                    self.project_root.join(wd)
                } else {
                    current_cwd.join(wd)
                };
                let canonical = tokio::fs::canonicalize(&path)
                    .await
                    .with_context(|| format!("Working directory not found: {}", wd))?;

                if confined && !canonical.starts_with(&self.canonical_project_root) {
                    anyhow::bail!("Working directory '{}' is outside project root", wd);
                }
                canonical
            }
            None => current_cwd,
        };

        Ok(base)
    }

    /// Advance the tracked cwd after a command that began with `cd`. Confined
    /// runs clamp any escape back to the project root; unconfined runs follow.
    pub(super) async fn update_cwd(&self, cwd: &Path, command: &str) {
        // `extract_cd_target` only recognizes a `cd <target>` prefix; a bare
        // `cd` (home) yields `None` and leaves the tracked cwd untouched.
        let Some(new_cwd) = extract_cd_target(command) else {
            return;
        };

        // Unconfined (yolo): track `cd` anywhere; confined: clamp below.
        if !self.yolo_mode.level().is_confined() {
            let path = Path::new(&new_cwd);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            if let Ok(canonical) = tokio::fs::canonicalize(&path).await {
                let mut current_cwd = self.cwd.lock().await;
                *current_cwd = canonical;
            }
            return;
        }

        if !is_safe_relative_path(Path::new(&new_cwd)) {
            let mut current_cwd = self.cwd.lock().await;
            *current_cwd = self.canonical_project_root.clone();
            return;
        }

        let path = cwd.join(&new_cwd);
        if let Ok(canonical) = tokio::fs::canonicalize(&path).await {
            let mut current_cwd = self.cwd.lock().await;
            *current_cwd = if canonical.starts_with(&self.canonical_project_root) {
                canonical
            } else {
                self.canonical_project_root.clone()
            };
        }
    }
}
