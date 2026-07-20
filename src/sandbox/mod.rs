//! OS-level confinement for the child shells the agent spawns.
//!
//! The sandbox is an enforcement **floor**: independent of the permission rules
//! and risk classifier, which remain policy *on top*. It confines the child
//! process only — never bonsai itself, which must keep writing to `~/.bonsai`,
//! the model catalog, and `.bonsai/tool-output`. It limits **writes** to a set of
//! allowed roots and can optionally **deny network**. Reads stay broad because
//! toolchains need them — secret-read redaction is a separate concern.
//!
//! Both shell spawn sites — foreground [`crate::tool`]`::BashTool::run_command`
//! and [`crate::background`]`::BackgroundTaskRegistry::start` — route through the
//! single entry point [`CommandSandbox::command`]. The holder mirrors
//! [`crate::yolo::YoloMode`]: one `Arc`-shared, atomically-toggled instance cloned
//! into both spawn sites plus the agent, so `/sandbox on|off` is visible
//! everywhere immediately.
//!
//! macOS uses Seatbelt via `sandbox-exec`; Linux uses Bubblewrap when available.
//! The default posture is on-when-available with network denied; users may
//! explicitly relax either setting through `/sandbox` or environment/config.

mod policy;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::process::Command;

pub(crate) use policy::SandboxPolicy;

/// Which OS mechanism confines child commands on this host, decided once at
/// startup by [`detect_backend`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxBackend {
    /// macOS `sandbox-exec` wrapper at `/usr/bin/sandbox-exec`. Constructed only
    /// on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    SeatbeltExec,
    /// Linux Bubblewrap wrapper at `/usr/bin/bwrap`. Constructed only on Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Bubblewrap { network_deny_supported: bool },
    /// No OS sandbox available (unsupported OS, or the backend tool is missing).
    Unavailable,
}

impl SandboxBackend {
    /// Whether this backend can actually confine a command.
    pub(crate) fn is_available(self) -> bool {
        !matches!(self, Self::Unavailable)
    }

    /// Whether this backend can enforce a network-deny policy.
    pub(crate) fn supports_network_deny(self) -> bool {
        matches!(
            self,
            Self::SeatbeltExec
                | Self::Bubblewrap {
                    network_deny_supported: true,
                }
        )
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SeatbeltExec => "seatbelt",
            Self::Bubblewrap { .. } => "bubblewrap",
            Self::Unavailable => "none",
        }
    }
}

/// Detect the best sandbox backend for this host. Returns
/// [`SandboxBackend::Unavailable`] on platforms without a working backend.
pub(crate) fn detect_backend() -> SandboxBackend {
    #[cfg(target_os = "macos")]
    {
        if Path::new("/usr/bin/sandbox-exec").exists() {
            return SandboxBackend::SeatbeltExec;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(backend) = linux::detect_backend() {
            return backend;
        }
    }
    SandboxBackend::Unavailable
}

/// The live sandbox state, cheaply cloneable and shared across the spawn sites
/// and the agent (mirrors [`crate::yolo::YoloMode`]). Toggling `enabled` /
/// `deny_network` is visible to every holder immediately.
#[derive(Clone, Debug)]
pub(crate) struct CommandSandbox {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    backend: SandboxBackend,
    enabled: AtomicBool,
    deny_network: AtomicBool,
    /// Owned for the sandbox lifetime so concurrent sessions never share temp
    /// scratch space. Dropping the last sandbox handle removes the directory.
    private_temp: Option<tempfile::TempDir>,
    /// Resolved once at construction (project root + private temp + configured extras);
    /// stable for the session, so per-spawn policy lookups don't re-hit the FS.
    writable_roots: Vec<PathBuf>,
}

impl CommandSandbox {
    /// Construct from a detected backend and the project root, with no `[sandbox]`
    /// config folded in — a test convenience; real callers (`bootstrap`,
    /// `headless`) always have a loaded [`Config`](crate::config::Config) and use
    /// [`Self::from_config`] instead. See [`Self::from_config`] for what the
    /// initial state is read from.
    #[cfg(test)]
    pub(crate) fn new(backend: SandboxBackend, project_root: &Path) -> Self {
        Self::from_config(
            backend,
            project_root,
            &crate::config::SandboxConfig::default(),
        )
    }

    /// Construct from a detected backend and the project root. Reads the initial
    /// enabled state from `BONSAI_SANDBOX` (default **on** when the backend is
    /// available) and the network posture from `BONSAI_SANDBOX_NETWORK`
    /// (default **denied**). Writable roots are the
    /// resolved project/temp/toolchain-cache set plus `BONSAI_SANDBOX_WRITABLE_ROOTS`
    /// (`policy::writable_roots`) further extended by the project's `[sandbox]`
    /// config: `config.writable_roots` unions in (like the env var), and
    /// `config.deny_network` seeds the initial posture when
    /// `BONSAI_SANDBOX_NETWORK` isn't set — env still wins either way.
    pub(crate) fn from_config(
        backend: SandboxBackend,
        project_root: &Path,
        config: &crate::config::SandboxConfig,
    ) -> Self {
        let want_enabled = env_flag("BONSAI_SANDBOX").unwrap_or_else(|| backend.is_available());
        let deny_network = env_deny_network().or(config.deny_network).unwrap_or(true);
        let private_temp = tempfile::Builder::new()
            .prefix("bonsai-session-")
            .tempdir()
            .map_err(|err| {
                tracing::warn!(error = %err, "failed to create private sandbox temp directory");
                err
            })
            .ok();
        let temp_root = private_temp
            .as_ref()
            .map(tempfile::TempDir::path)
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let mut writable_roots = policy::writable_roots(project_root, &temp_root);
        for root in &config.writable_roots {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if !writable_roots.contains(&canonical) {
                writable_roots.push(canonical);
            }
        }
        Self {
            inner: Arc::new(Inner {
                backend,
                enabled: AtomicBool::new(want_enabled && backend.is_available()),
                deny_network: AtomicBool::new(deny_network),
                private_temp,
                writable_roots,
            }),
        }
    }

    /// A disabled, no-op sandbox — the default for tests and any path with no real
    /// policy (the agent's placeholder background registry, the eval runner).
    ///
    /// Invariant: a disabled sandbox is the `Unavailable` backend with no enabled
    /// state and no writable roots, so [`is_active`](Self::is_active) is always
    /// `false` and it never confines. This is deliberately a distinct constructor
    /// rather than `new(Unavailable, "")` so it reads no env and resolves no roots.
    pub(crate) fn disabled() -> Self {
        Self {
            inner: Arc::new(Inner {
                backend: SandboxBackend::Unavailable,
                enabled: AtomicBool::new(false),
                deny_network: AtomicBool::new(false),
                private_temp: None,
                writable_roots: Vec::new(),
            }),
        }
    }

    pub(crate) fn backend(&self) -> SandboxBackend {
        self.inner.backend
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn set_enabled(&self, on: bool) {
        self.inner.enabled.store(on, Ordering::Relaxed);
    }

    pub(crate) fn deny_network(&self) -> bool {
        self.inner.deny_network.load(Ordering::Relaxed)
    }

    pub(crate) fn set_deny_network(&self, on: bool) {
        self.inner.deny_network.store(on, Ordering::Relaxed);
    }

    /// Whether confinement will be applied for the next spawn — purely a function
    /// of the toggle and backend availability. **Independent of the autonomy
    /// level**: the sandbox is an enforcement *floor*, so it stays on even at
    /// `yolo` once enabled. Leaving it is explicit (`/sandbox off`) rather than an
    /// implicit side-effect of how much the agent auto-approves.
    pub(crate) fn is_active(&self) -> bool {
        self.is_enabled() && self.inner.backend.is_available()
    }

    /// The policy that would be enforced right now: writes confined to the
    /// resolved writable roots (project root + temp + configured extras) plus the
    /// live network posture.
    pub(crate) fn policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            writable_roots: self.inner.writable_roots.clone(),
            deny_network: self.deny_network(),
        }
    }

    /// Private scratch directory exported to child commands. Disabled test/eval
    /// sandboxes return `None` and inherit the caller's environment unchanged.
    pub(crate) fn temp_dir(&self) -> Option<&Path> {
        self.inner
            .private_temp
            .as_ref()
            .map(tempfile::TempDir::path)
    }

    /// Build the [`Command`] to run `shell -c <script>` in `cwd`, confined per the
    /// live policy when the sandbox [`is_active`](Self::is_active). The caller
    /// still wires stdio / `kill_on_drop` / process group and spawns.
    pub(crate) fn command(
        &self,
        shell: &str,
        script: &str,
        cwd: &Path,
    ) -> (Command, SpawnDecision) {
        let (mut command, decision) = if self.is_active()
            && let Some(confined) = self.wrap_active(shell, script, cwd)
        {
            confined
        } else {
            (
                plain_command(shell, script, cwd),
                SpawnDecision::unconfined(self.inner.backend),
            )
        };
        self.configure_temp_environment(&mut command);
        (command, decision)
    }

    /// Build the same `shell -c <script>` command WITHOUT the sandbox wrapper,
    /// for a single user-approved, audited escape. Does NOT touch the shared
    /// `enabled` flag — confinement still applies to every other command. The
    /// returned [`SpawnDecision`] is flagged `escaped` so `log()` warns.
    pub(crate) fn command_unconfined(
        &self,
        shell: &str,
        script: &str,
        cwd: &Path,
    ) -> (Command, SpawnDecision) {
        let mut command = plain_command(shell, script, cwd);
        self.configure_temp_environment(&mut command);
        (command, SpawnDecision::escaped(self.inner.backend))
    }

    fn configure_temp_environment(&self, command: &mut Command) {
        let Some(temp_dir) = self.temp_dir() else {
            return;
        };
        command
            .env("TMPDIR", temp_dir)
            .env("TMP", temp_dir)
            .env("TEMP", temp_dir)
            .env("npm_config_cache", temp_dir.join("cache/npm"))
            .env("XDG_CACHE_HOME", temp_dir.join("cache/xdg"))
            .env("PIP_CACHE_DIR", temp_dir.join("cache/pip"))
            .env("GOCACHE", temp_dir.join("cache/go-build"));
    }

    #[cfg(target_os = "macos")]
    fn wrap_active(
        &self,
        shell: &str,
        script: &str,
        cwd: &Path,
    ) -> Option<(Command, SpawnDecision)> {
        match self.inner.backend {
            SandboxBackend::SeatbeltExec => Some(macos::wrap(shell, script, cwd, &self.policy())),
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    fn wrap_active(
        &self,
        shell: &str,
        script: &str,
        cwd: &Path,
    ) -> Option<(Command, SpawnDecision)> {
        match self.inner.backend {
            SandboxBackend::Bubblewrap { .. } => Some(linux::wrap(
                shell,
                script,
                cwd,
                self.inner.backend,
                &self.policy(),
            )),
            _ => None,
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn wrap_active(
        &self,
        _shell: &str,
        _script: &str,
        _cwd: &Path,
    ) -> Option<(Command, SpawnDecision)> {
        None
    }
}

/// The unconfined `shell -c <script>` command, used when the sandbox is inactive.
fn plain_command(shell: &str, script: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new(shell);
    cmd.arg("-c").arg(script).current_dir(cwd);
    cmd
}

/// What [`CommandSandbox::command`] decided for one spawn — for tracing and, in a
/// later increment, `/doctor` / status reporting.
#[derive(Clone, Debug)]
pub(crate) struct SpawnDecision {
    pub confined: bool,
    pub backend: SandboxBackend,
    pub network_denied: bool,
    /// Set when requested confinement couldn't be fully honored (e.g. network
    /// deny on a backend that can't enforce it). Filesystem confinement still
    /// applies; the reason is surfaced via [`log`](Self::log).
    pub degraded: Option<String>,
    /// This command was deliberately spawned OUTSIDE the sandbox via an approved,
    /// audited escape (see [`CommandSandbox::command_unconfined`]). Distinct from
    /// `confined == false` because the sandbox was *active* and was stepped past.
    pub escaped: bool,
}

impl SpawnDecision {
    fn unconfined(backend: SandboxBackend) -> Self {
        Self {
            confined: false,
            backend,
            network_denied: false,
            degraded: None,
            escaped: false,
        }
    }

    /// A command spawned outside an *active* sandbox by an approved escape.
    fn escaped(backend: SandboxBackend) -> Self {
        Self {
            confined: false,
            backend,
            network_denied: false,
            degraded: None,
            escaped: true,
        }
    }

    /// Emit a trace line for this spawn; warn loudly on degradation. An approved
    /// escape is audited at the bash-tool level (with the command + grant scope),
    /// so this layer only debug-traces it to avoid a duplicate warn line.
    pub(crate) fn log(&self) {
        if let Some(reason) = &self.degraded {
            tracing::warn!(reason = %reason, backend = self.backend.label(), "sandbox degraded");
        }
        if self.escaped {
            tracing::debug!(
                backend = self.backend.label(),
                "spawned outside the sandbox"
            );
        }
        if self.confined {
            tracing::debug!(
                backend = self.backend.label(),
                network_denied = self.network_denied,
                "command sandboxed",
            );
        }
    }
}

/// Parse a boolean-ish env var (`1/on/true/yes` → true, `0/off/false/no` → false).
fn env_flag(var: &str) -> Option<bool> {
    let value = std::env::var(var).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" | "enabled" => Some(true),
        "0" | "off" | "false" | "no" | "disabled" => Some(false),
        _ => None,
    }
}

/// Parse `BONSAI_SANDBOX_NETWORK` into a deny-network flag (`deny`/`block` → true,
/// `allow` → false).
fn env_deny_network() -> Option<bool> {
    let value = std::env::var("BONSAI_SANDBOX_NETWORK").ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "deny" | "denied" | "block" | "off" => Some(true),
        "allow" | "allowed" | "on" => Some(false),
        _ => None,
    }
}
