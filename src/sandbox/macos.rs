//! macOS Seatbelt backend: wrap the child in `sandbox-exec` with a generated
//! SBPL profile that confines writes to the policy's roots and optionally denies
//! network. `sandbox-exec` does `sandbox_init()` then `execvp`, replacing its own
//! process image — so the child PID *is* the shell and the existing
//! `kill_on_drop` / process-group termination reaches it exactly as before.

use std::path::Path;

use tokio::process::Command;

use super::policy::SandboxPolicy;
use super::{SandboxBackend, SpawnDecision};

/// Build a `sandbox-exec` command running `shell -c <script>` under a profile
/// generated from `policy`.
pub(super) fn wrap(
    shell: &str,
    script: &str,
    cwd: &Path,
    policy: &SandboxPolicy,
) -> (Command, SpawnDecision) {
    let profile = generate_profile(policy);
    let mut cmd = Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p")
        .arg(profile)
        .arg("--")
        .arg(shell)
        .arg("-c")
        .arg(script)
        .current_dir(cwd);
    (
        cmd,
        SpawnDecision {
            confined: true,
            backend: SandboxBackend::SeatbeltExec,
            network_denied: policy.deny_network,
            degraded: None,
            escaped: false,
        },
    )
}

/// Generate the SBPL profile. Posture: import the system policy (`allow
/// default`), deny *all* writes, then re-allow writes under the policy roots and
/// the device files a shell needs. SBPL is **last-match-wins**, so the order
/// (allow default → deny file-write* → allow file-write* allowlist) is load-bearing.
pub(super) fn generate_profile(policy: &SandboxPolicy) -> String {
    let mut out = String::new();
    out.push_str("(version 1)\n");
    out.push_str("(allow default)\n");
    out.push_str("(deny file-write*)\n");
    out.push_str("(allow file-write*\n");
    for root in &policy.writable_roots {
        out.push_str("    (subpath \"");
        out.push_str(&escape(&root.to_string_lossy()));
        out.push_str("\")\n");
    }
    // Device files a shell + its redirections need even under the blanket
    // file-write deny (the standard sandbox device allowlist).
    for dev in ["/dev/null", "/dev/zero", "/dev/tty"] {
        out.push_str("    (literal \"");
        out.push_str(dev);
        out.push_str("\")\n");
    }
    out.push_str("    (subpath \"/dev/fd\")\n");
    out.push_str("    (regex #\"^/dev/ttys[0-9]*$\")\n");
    out.push_str(")\n");
    if policy.deny_network {
        out.push_str("(deny network*)\n");
    }
    out
}

/// Escape a path for an SBPL string literal (`\` and `"`).
fn escape(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}
