//! The [`SandboxPolicy`] a single spawn is confined to, plus resolution of the
//! session's writable roots.

use std::path::{Path, PathBuf};

/// What the sandbox enforces for one spawned child. Reads stay broad; only
/// **writes** are confined to `writable_roots`, plus optional network deny.
#[derive(Clone, Debug)]
pub(crate) struct SandboxPolicy {
    /// Canonicalized roots the child may write under.
    pub writable_roots: Vec<PathBuf>,
    /// Deny all network egress on backends that can enforce it.
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    pub deny_network: bool,
}

/// Resolve the full writable-root set for a session: the project root, its
/// private temp dir, the repository's git common dir when it lives elsewhere,
/// the two shared dependency stores that common build tools
/// cannot layer read-only, and any
/// `BONSAI_SANDBOX_WRITABLE_ROOTS` extras — canonicalized and de-duplicated.
/// Computed once at construction (these don't change for a session), so per-spawn
/// policy lookups don't re-hit the filesystem.
pub(crate) fn writable_roots(project_root: &Path, temp_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    push_canonical(&mut roots, project_root.to_path_buf());
    push_canonical(&mut roots, temp_root.to_path_buf());
    if let Some(git_dir) = external_git_common_dir(project_root) {
        push_canonical_outside(&mut roots, git_dir);
    }
    extend_canonical(&mut roots, shared_dependency_cache_roots());
    extend_canonical(&mut roots, extra_writable_roots_from_env());
    roots
}

/// The repository's git *common* directory, resolved so it can be added to the
/// writable roots when it lives outside them. In a plain checkout `.git` sits
/// under the project root and the containment check drops it; in a linked
/// worktree (`--isolation worktree`, `git worktree add`) — or with
/// `--separate-git-dir`, or a session opened inside a submodule — commits
/// write to the source repository's git dir, which would otherwise be denied
/// and push the model toward sandbox escapes for plain `git add`/`commit`.
/// Widening covers only the repo's own git state, never the source working
/// tree; hooks and filters keep running confined.
fn external_git_common_dir(project_root: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--git-common-dir"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let path = Path::new(text.trim());
    if path.as_os_str().is_empty() {
        return None;
    }
    // rev-parse prints a relative path (".git") when the common dir is inside
    // the directory it ran in; anchor it to the project root either way.
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    })
}

/// Shared dependency stores that remain writable by design. Cargo and Go mix
/// immutable downloaded dependencies with resolver locks/metadata in one home,
/// and neither exposes a reliable read-only-base + writable-overlay contract.
/// Keeping only these two stores writable preserves ordinary locked builds.
/// Ephemeral npm, XDG, pip, and Go build caches are redirected into the private
/// per-session temp root by `CommandSandbox::configure_temp_environment`; rustup
/// is read-only under the sandbox and requires an explicit escape to update.
fn shared_dependency_cache_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let home = home_dir();
    let home = home.as_deref();
    push_env_or_home(&mut roots, home, "CARGO_HOME", ".cargo");
    if let Some(path) = env_path("GOMODCACHE") {
        push_canonical(&mut roots, path);
    }
    roots
}

/// Push `$<env>` if set, else `<home>/<sub>` (when home is known), canonicalized.
fn push_env_or_home(roots: &mut Vec<PathBuf>, home: Option<&Path>, env: &str, sub: &str) {
    if let Some(path) = env_path(env) {
        push_canonical(roots, path);
    } else if let Some(home) = home {
        push_canonical(roots, home.join(sub));
    }
}

/// The user's home directory, if known (unix `$HOME`; the sandbox backends are
/// unix-only).
fn home_dir() -> Option<PathBuf> {
    env_path("HOME")
}

/// A non-empty path-valued env var.
fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Extra writable roots from `BONSAI_SANDBOX_WRITABLE_ROOTS` (colon-separated).
/// The env-only stand-in until project config (`.bonsai/config.toml`) lands; it
/// lets a user widen the write surface for any additional dir.
fn extra_writable_roots_from_env() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(value) = std::env::var("BONSAI_SANDBOX_WRITABLE_ROOTS") {
        for entry in value.split(':') {
            let entry = entry.trim();
            if !entry.is_empty() {
                push_canonical(&mut roots, PathBuf::from(entry));
            }
        }
    }
    roots
}

/// Canonicalize and de-dup into `roots`. Falls back to the raw path when it can't
/// be canonicalized (e.g. it doesn't exist yet) so the rule is still emitted.
fn push_canonical(roots: &mut Vec<PathBuf>, path: PathBuf) {
    let canonical = path.canonicalize().unwrap_or(path);
    if !roots.contains(&canonical) {
        roots.push(canonical);
    }
}

/// Push `path` (canonicalized) only when no already-collected root contains
/// it — a `.git` under the project root needs no rule of its own.
fn push_canonical_outside(roots: &mut Vec<PathBuf>, path: PathBuf) {
    let canonical = path.canonicalize().unwrap_or(path);
    if !roots.iter().any(|root| canonical.starts_with(root)) {
        roots.push(canonical);
    }
}

/// De-dup `more` (already canonical) into the accumulating `roots`.
fn extend_canonical(roots: &mut Vec<PathBuf>, more: Vec<PathBuf>) {
    for root in more {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_roots_include_only_shared_dependency_caches_and_project() {
        let proj = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let roots = writable_roots(proj.path(), temp.path());
        assert!(roots.contains(&proj.path().canonicalize().unwrap()));
        assert!(roots.contains(&temp.path().canonicalize().unwrap()));

        let caches = shared_dependency_cache_roots();
        assert!(
            !caches.is_empty(),
            "expected at least the cargo/rustup caches to resolve",
        );
        for cache in caches {
            assert!(
                roots.contains(&cache),
                "toolchain cache {cache:?} missing from writable roots",
            );
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.email=test@test",
                "-c",
                "user.name=test",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    #[test]
    fn linked_worktree_gets_the_source_git_dir_as_a_writable_root() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "--quiet"]);
        std::fs::write(source.path().join("file"), "x").unwrap();
        git(source.path(), &["add", "file"]);
        git(source.path(), &["commit", "--quiet", "-m", "init"]);
        let worktree = source.path().join("wt");
        git(
            source.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "wt",
            ],
        );

        let temp = tempfile::tempdir().unwrap();
        let roots = writable_roots(&worktree, temp.path());
        let common = source.path().join(".git").canonicalize().unwrap();
        assert!(
            roots.contains(&common),
            "worktree session must be able to write the source git dir: {roots:?}"
        );
        assert!(roots.contains(&worktree.canonicalize().unwrap()));
    }

    #[test]
    fn plain_checkout_adds_no_separate_git_root() {
        let proj = tempfile::tempdir().unwrap();
        git(proj.path(), &["init", "--quiet"]);

        let temp = tempfile::tempdir().unwrap();
        let roots = writable_roots(proj.path(), temp.path());
        let git_dir = proj.path().join(".git").canonicalize().unwrap();
        // Covered by the project root; a separate rule would be noise.
        assert!(
            !roots.contains(&git_dir),
            "in-tree .git must not become its own root: {roots:?}"
        );
    }

    #[test]
    fn shared_dependency_caches_resolve_cargo_home() {
        // CARGO_HOME (or the ~/.cargo fallback) must be present so `cargo build`
        // can write its registry/cache under the sandbox.
        let caches = shared_dependency_cache_roots();
        let cargo = env_path("CARGO_HOME")
            .or_else(|| home_dir().map(|home| home.join(".cargo")))
            .map(|path| path.canonicalize().unwrap_or(path));
        if let Some(cargo) = cargo {
            assert!(
                caches.contains(&cargo),
                "cargo cache {cargo:?} not in {caches:?}"
            );
        }
    }
}
