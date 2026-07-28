//! Linux Bubblewrap backend. Bubblewrap gives bonsai the same wrapper shape as
//! macOS `sandbox-exec`: configure the child sandbox, then run `shell -c`.

use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use tokio::process::Command;

use super::policy::SandboxPolicy;
use super::{SandboxBackend, SpawnDecision};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
enum NetworkMode {
    Shared,
    Isolated,
}

/// Detect the Bubblewrap capabilities available on this host.
pub(super) fn detect_backend() -> Option<SandboxBackend> {
    let executable = super::resolve_executable("bwrap")?;
    if !probe_bubblewrap(&executable, NetworkMode::Shared) {
        return None;
    }

    Some(SandboxBackend::Bubblewrap {
        network_deny_supported: probe_bubblewrap(&executable, NetworkMode::Isolated),
        executable,
    })
}

fn probe_bubblewrap(executable: &Path, network: NetworkMode) -> bool {
    let mut command = StdCommand::new(executable);
    command.arg("--unshare-all");
    if matches!(network, NetworkMode::Shared) {
        command.arg("--share-net");
    }
    command
        .arg("--die-with-parent")
        .arg("--ro-bind")
        .arg("/")
        .arg("/")
        .arg("--dev")
        .arg("/dev")
        .arg("--proc")
        .arg("/proc")
        .arg("--chdir")
        .arg("/")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() >= PROBE_TIMEOUT => {
                if let Err(err) = child.kill() {
                    tracing::debug!(%err, "sandbox probe child already dead (kill)");
                }
                if let Err(err) = child.wait() {
                    tracing::debug!(%err, "sandbox probe child already reaped (wait)");
                }
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                if let Err(err) = child.kill() {
                    tracing::debug!(%err, "sandbox probe child already dead (kill)");
                }
                if let Err(err) = child.wait() {
                    tracing::debug!(%err, "sandbox probe child already reaped (wait)");
                }
                return false;
            }
        }
    }
}

/// Build a Bubblewrap command running `shell -c <script>` under `policy`.
pub(super) fn wrap(
    shell: &str,
    script: &str,
    cwd: &Path,
    backend: SandboxBackend,
    policy: &SandboxPolicy,
) -> (Command, SpawnDecision) {
    let SandboxBackend::Bubblewrap { executable, .. } = &backend else {
        unreachable!("Bubblewrap wrapper requires a Bubblewrap backend");
    };
    let mut cmd = Command::new(executable);
    cmd.arg("--unshare-all")
        .arg("--die-with-parent")
        .arg("--ro-bind")
        .arg("/")
        .arg("/");

    for root in &policy.writable_roots {
        cmd.arg("--bind-try").arg(root).arg(root);
    }

    let network_denied = policy.deny_network && backend.supports_network_deny();
    if !network_denied {
        cmd.arg("--share-net");
    }

    cmd.arg("--dev")
        .arg("/dev")
        .arg("--proc")
        .arg("/proc")
        .arg("--chdir")
        .arg(cwd)
        .arg("--")
        .arg(shell)
        .arg("-c")
        .arg(script)
        .current_dir(cwd);

    (
        cmd,
        SpawnDecision {
            confined: true,
            backend: backend.clone(),
            network_denied,
            degraded: network_degradation(policy, backend),
            escaped: false,
        },
    )
}

fn network_degradation(policy: &SandboxPolicy, backend: SandboxBackend) -> Option<String> {
    if policy.deny_network && !backend.supports_network_deny() {
        Some("network deny requested but Bubblewrap cannot isolate networking on this host".into())
    } else {
        None
    }
}
