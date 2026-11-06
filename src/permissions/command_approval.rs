//! Exact command approvals, separate from operator-authored glob rules.

use std::borrow::Cow;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

const NAMESPACE: &str = "bonsai-exact-command-";
const PREFIX: &str = "bonsai-exact-command-v1:";

/// Stored descriptions are redacted; only the fingerprint matches a request.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandApproval {
    command: String,
    cwd: String,
    fingerprint: String,
}

impl CommandApproval {
    pub(super) fn new(cwd: &Path, command: &str) -> Self {
        let display = crate::util::ansi::strip_terminal_controls(command);
        let cwd_display = cwd.to_string_lossy();
        let cwd_display = crate::util::ansi::strip_terminal_controls(cwd_display.as_ref());
        Self {
            command: crate::redact::redact(display.as_ref()).into_owned(),
            cwd: crate::redact::redact(cwd_display.as_ref()).into_owned(),
            fingerprint: fingerprint(cwd, command),
        }
    }

    pub(super) fn encode(&self) -> Result<String> {
        Ok(format!("{PREFIX}{}", serde_json::to_string(self)?))
    }

    pub(super) fn is_reserved(pattern: &str) -> bool {
        let Some(rest) = pattern
            .strip_prefix(NAMESPACE)
            .and_then(|s| s.strip_prefix('v'))
        else {
            return false;
        };
        let Some((version, _)) = rest.split_once(':') else {
            return false;
        };
        !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
    }

    pub(super) fn decode(pattern: &str) -> Option<Self> {
        let approval: Self = serde_json::from_str(pattern.strip_prefix(PREFIX)?).ok()?;
        (approval.fingerprint.len() == 64
            && approval
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()))
        .then_some(approval)
    }

    pub(super) fn matches(&self, fingerprint: &str) -> bool {
        self.fingerprint == fingerprint
    }
}

pub(super) fn fingerprint(cwd: &Path, command: &str) -> String {
    let cwd = cwd.as_os_str().as_encoded_bytes();
    let mut hash = blake3::Hasher::new();
    hash.update(PREFIX.as_bytes());
    hash.update(&(cwd.len() as u64).to_le_bytes());
    hash.update(cwd);
    hash.update(command.as_bytes());
    hash.finalize().to_hex().to_string()
}

/// Describe the granted command and directory without exposing storage keys.
pub(crate) fn display_permission_pattern(pattern: &str) -> Cow<'_, str> {
    match CommandApproval::decode(pattern) {
        Some(approval) => Cow::Owned(format!(
            "exact command in {}: {}",
            approval.cwd, approval.command
        )),
        None => Cow::Borrowed(pattern),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_replay_binds_original_bytes_and_directory() {
        let cwd = Path::new("/project/a");
        let command = "echo 'one; two' && echo '*'";
        let approval = CommandApproval::new(cwd, command);
        assert!(approval.matches(&fingerprint(cwd, command)));
        for (directory, changed) in [
            (cwd, "echo one; two && echo '*'"),
            (cwd, "echo 'one; two' && echo other"),
            (Path::new("/project/b"), command),
            (cwd, "FLAG=changed echo 'one; two' && echo '*'"),
        ] {
            assert!(!approval.matches(&fingerprint(directory, changed)));
        }
        assert_ne!(
            fingerprint(Path::new("/ab"), "c"),
            fingerprint(Path::new("/a"), "bc")
        );
    }

    #[test]
    fn approval_replay_redacts_storage_but_keeps_exact_identity() {
        let secret = "abcdef0123456789".repeat(3);
        let command = format!("curl -H 'Authorization: Bearer {secret}' https://example.com");
        let approval = CommandApproval::new(Path::new("/project"), &command);
        let encoded = approval.encode().unwrap();
        assert!(!encoded.contains(&secret));
        let restored = CommandApproval::decode(&encoded).unwrap();
        assert!(restored.matches(&fingerprint(Path::new("/project"), &command)));
        let label = display_permission_pattern(&encoded);
        assert!(label.starts_with("exact command in /project: curl"));
        assert!(!label.contains(&secret));
        assert!(!label.contains(&approval.fingerprint));
    }

    #[test]
    fn approval_replay_rejects_invalid_or_future_records() {
        for raw in [
            "bonsai-exact-command-v2:{}",
            "bonsai-exact-command-v1:{}",
            "bonsai-exact-command-v1:{\"command\":\"x\",\"cwd\":\"/\",\"fingerprint\":\"\"}",
        ] {
            assert!(CommandApproval::is_reserved(raw));
            assert!(CommandApproval::decode(raw).is_none());
        }
        assert_eq!(display_permission_pattern("cargo test *"), "cargo test *");
    }
}
