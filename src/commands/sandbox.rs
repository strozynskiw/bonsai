//! `/sandbox` — toggle the OS-level command sandbox (the enforcement floor)
//! and its network posture. Orthogonal to `/autonomy`: autonomy decides whether a
//! command runs without a prompt; the sandbox decides what a command can
//! physically do (confine writes, optionally deny network).

use crate::sandbox::CommandSandbox;

pub(crate) const SANDBOX_USAGE: &str = "Usage: /sandbox [on|off|net on|net off|status]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxCommandRequest {
    /// Enable or disable confinement.
    Set(bool),
    /// Set the network-deny posture.
    SetNet(bool),
    /// Report current state.
    Status,
}

pub(crate) fn parse_sandbox_command(input: &str) -> Result<SandboxCommandRequest, String> {
    let args = super::command_args(input, "/sandbox", SANDBOX_USAGE)?;
    match args.as_slice() {
        [] | ["status"] => Ok(SandboxCommandRequest::Status),
        ["on"] => Ok(SandboxCommandRequest::Set(true)),
        ["off"] => Ok(SandboxCommandRequest::Set(false)),
        ["net", "on"] => Ok(SandboxCommandRequest::SetNet(true)),
        ["net", "off"] => Ok(SandboxCommandRequest::SetNet(false)),
        _ => Err(SANDBOX_USAGE.to_string()),
    }
}

/// Message when enabling is requested but no backend exists on this host.
pub(crate) fn sandbox_unavailable_message() -> String {
    "Sandbox unavailable: no OS backend on this platform. Nothing was enabled.".to_string()
}

pub(crate) fn sandbox_set_message(on: bool, sandbox: &CommandSandbox) -> String {
    if on {
        format!(
            "Sandbox enabled ({}). Writes confined to the project; network {}.",
            sandbox.backend().label(),
            network_state(sandbox),
        )
    } else {
        "Sandbox disabled.".to_string()
    }
}

pub(crate) fn sandbox_net_message(on: bool, sandbox: &CommandSandbox) -> String {
    let word = network_word(on);
    if on && !sandbox.backend().supports_network_deny() {
        format!(
            "Network egress set to {word}, but the active backend ({}) can't enforce it.",
            sandbox.backend().label(),
        )
    } else {
        format!("Network egress {word}.")
    }
}

#[cfg(test)]
fn sandbox_status_message(sandbox: &CommandSandbox) -> String {
    let policy = sandbox.policy();
    let state = if sandbox.is_enabled() {
        "enabled"
    } else {
        "disabled"
    };
    // Flag a network-deny intent the active backend can't actually enforce, so it
    // isn't silently non-functional.
    let network = if policy.deny_network && !sandbox.backend().supports_network_deny() {
        "denied (unenforced)"
    } else {
        network_word(policy.deny_network)
    };
    let roots = policy
        .writable_roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Sandbox: {state} ({}) · network: {network} · writable: {roots}",
        sandbox.backend().label(),
    )
}

fn network_word(deny: bool) -> &'static str {
    if deny { "denied" } else { "allowed" }
}

fn network_state(sandbox: &CommandSandbox) -> &'static str {
    if sandbox.deny_network() && !sandbox.backend().supports_network_deny() {
        "denied (unenforced)"
    } else {
        network_word(sandbox.deny_network())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toggles_status_and_network() {
        assert_eq!(
            parse_sandbox_command("/sandbox"),
            Ok(SandboxCommandRequest::Status)
        );
        assert_eq!(
            parse_sandbox_command("/sandbox status"),
            Ok(SandboxCommandRequest::Status)
        );
        assert_eq!(
            parse_sandbox_command("/sandbox on"),
            Ok(SandboxCommandRequest::Set(true))
        );
        assert_eq!(
            parse_sandbox_command("/sandbox off"),
            Ok(SandboxCommandRequest::Set(false))
        );
        assert_eq!(
            parse_sandbox_command("/sandbox net on"),
            Ok(SandboxCommandRequest::SetNet(true))
        );
        assert_eq!(
            parse_sandbox_command("/sandbox net off"),
            Ok(SandboxCommandRequest::SetNet(false))
        );
        assert!(parse_sandbox_command("/sandbox bogus").is_err());
        assert!(parse_sandbox_command("/sandbox net").is_err());
    }

    #[test]
    fn set_message_reflects_state() {
        let sandbox = CommandSandbox::disabled();
        assert_eq!(sandbox_set_message(false, &sandbox), "Sandbox disabled.");
        assert!(sandbox_set_message(true, &sandbox).starts_with("Sandbox enabled"));
    }

    #[test]
    fn set_message_flags_unenforced_network_deny() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = CommandSandbox::new(
            crate::sandbox::SandboxBackend::test_bubblewrap(false),
            tmp.path(),
        );
        sandbox.set_deny_network(true);

        let message = sandbox_set_message(true, &sandbox);
        assert!(message.contains("network denied (unenforced)"), "{message}");
    }

    #[test]
    fn status_message_lists_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = CommandSandbox::new(crate::sandbox::detect_backend(), tmp.path());
        let message = sandbox_status_message(&sandbox);
        assert!(message.contains("Sandbox:"));
        assert!(message.contains("writable:"));
    }

    #[test]
    fn status_flags_unenforceable_network_deny() {
        // Unavailable backend can't enforce a network-deny intent — status must
        // say so rather than claim "denied".
        let sandbox = CommandSandbox::disabled();
        sandbox.set_deny_network(true);
        let message = sandbox_status_message(&sandbox);
        assert!(
            message.contains("network: denied (unenforced)"),
            "{message}"
        );
    }
}
