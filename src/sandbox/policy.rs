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
/// private temp dir, the two shared dependency stores that common build tools
/// cannot layer read-only, and any
/// `BONSAI_SANDBOX_WRITABLE_ROOTS` extras — canonicalized and de-duplicated.
/// Computed once at construction (these don't change for a session), so per-spawn
/// policy lookups don't re-hit the filesystem.
pub(crate) fn writable_roots(project_root: &Path, temp_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    push_canonical(&mut roots, project_root.to_path_buf());
    push_canonical(&mut roots, temp_root.to_path_buf());
    extend_canonical(&mut roots, shared_dependency_cache_roots());
    extend_canonical(&mut roots, extra_writable_roots_from_env());
    roots
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
