//! Policy over the config-declared [`DeclaredCapabilities`] data
//! (`src/config/schema.rs`): what risk tier and batching posture a capability
//! declaration implies. Capabilities come from the user's config, never from
//! the server — a remote MCP server does not get to self-declare its own
//! trust level.

use crate::config::{BatchingPolicy, Capability, DeclaredCapabilities};
use crate::tool::{ParallelPolicy, RiskTier};

impl DeclaredCapabilities {
    /// Undeclared ⇒ `High` (the roadmap's "unknown external tools default to
    /// serialized execution and an explicit permission gate" floor). `read`
    /// alone ⇒ `Low` — auto-approvable at Balanced, mirroring the
    /// `WEB_FETCH_TIER` precedent, but never at Conservative (which only
    /// auto-approves `ReadOnly`). Any `write`/`network`/`shell`/
    /// `untrusted_output` ⇒ `High`. An explicit `irreversible` declaration
    /// maps to `Destructive`, so it keeps the always-prompt floor at every
    /// autonomy level below yolo.
    pub(crate) fn risk_tier(&self) -> RiskTier {
        if self.capabilities.is_empty() {
            return RiskTier::High;
        }
        if self.capabilities.contains(&Capability::Irreversible) {
            return RiskTier::Destructive;
        }
        if self
            .capabilities
            .iter()
            .all(|capability| *capability == Capability::Read)
        {
            return RiskTier::Low;
        }
        RiskTier::High
    }

    /// `Serialized` unless `batching = "path_scoped"` AND the declared
    /// capabilities are a subset of `{read, write}` — network/shell side
    /// effects aren't expressible as a path conflict, so they always
    /// serialize regardless of the configured batching policy.
    pub(crate) fn parallel_policy(&self) -> ParallelPolicy {
        let path_safe = self
            .capabilities
            .iter()
            .all(|capability| matches!(capability, Capability::Read | Capability::Write));
        if self.batching == BatchingPolicy::PathScoped && path_safe {
            ParallelPolicy::PathScoped
        } else {
            ParallelPolicy::Serialized
        }
    }
}

/// `Vec<Capability>` rendered as their `[serde(rename_all = "snake_case")]`
/// wire labels, for the extension-tool permission prompt and `/config`.
pub(crate) fn capability_labels(capabilities: &[Capability]) -> Vec<String> {
    capabilities
        .iter()
        .map(|capability| capability_label(*capability).to_string())
        .collect()
}

/// `[serde(rename_all = "snake_case")]` label for one capability, shared by
/// `/config` and the extension-tool permission prompt so both name a
/// capability the same way.
pub(crate) fn capability_label(capability: Capability) -> &'static str {
    match capability {
        Capability::Read => "read",
        Capability::Write => "write",
        Capability::Network => "network",
        Capability::Shell => "shell",
        Capability::Irreversible => "irreversible",
        Capability::UntrustedOutput => "untrusted_output",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeclaredCapabilities;

    fn caps(capabilities: &[Capability], batching: BatchingPolicy) -> DeclaredCapabilities {
        DeclaredCapabilities {
            capabilities: capabilities.to_vec(),
            batching,
        }
    }

    #[test]
    fn undeclared_capabilities_are_high_risk_and_serialized() {
        let declared = caps(&[], BatchingPolicy::Serialized);
        assert_eq!(declared.risk_tier(), RiskTier::High);
        assert_eq!(declared.parallel_policy(), ParallelPolicy::Serialized);
    }

    #[test]
    fn read_only_is_low_risk() {
        let declared = caps(&[Capability::Read], BatchingPolicy::Serialized);
        assert_eq!(declared.risk_tier(), RiskTier::Low);
    }

    #[test]
    fn write_network_or_shell_is_high_risk() {
        for capability in [Capability::Write, Capability::Network, Capability::Shell] {
            let declared = caps(&[capability], BatchingPolicy::Serialized);
            assert_eq!(declared.risk_tier(), RiskTier::High, "{capability:?}");
        }
        // Read mixed with anything else is no longer "read alone".
        let mixed = caps(
            &[Capability::Read, Capability::Network],
            BatchingPolicy::Serialized,
        );
        assert_eq!(mixed.risk_tier(), RiskTier::High);
    }

    #[test]
    fn irreversible_capability_keeps_the_destructive_floor() {
        let declared = caps(&[Capability::Irreversible], BatchingPolicy::Serialized);
        assert_eq!(declared.risk_tier(), RiskTier::Destructive);
    }

    #[test]
    fn path_scoped_batching_requires_read_write_only_capabilities() {
        let read_write = caps(
            &[Capability::Read, Capability::Write],
            BatchingPolicy::PathScoped,
        );
        assert_eq!(read_write.parallel_policy(), ParallelPolicy::PathScoped);

        // path_scoped is requested but network is not path-shaped, so it stays
        // serialized regardless of the configured batching policy.
        let with_network = caps(
            &[Capability::Read, Capability::Network],
            BatchingPolicy::PathScoped,
        );
        assert_eq!(with_network.parallel_policy(), ParallelPolicy::Serialized);

        // path_scoped capabilities but the server didn't opt in via batching.
        let opted_out = caps(
            &[Capability::Read, Capability::Write],
            BatchingPolicy::Serialized,
        );
        assert_eq!(opted_out.parallel_policy(), ParallelPolicy::Serialized);
    }
}
