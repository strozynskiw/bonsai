//! The self-review-before-done policy: whether the coding agent runs a
//! critique pass over its own diff before declaring a task complete.
//!
//! This is distinct from the test-driven verify loop — it is a reasoning
//! critique ("did I actually satisfy the ask, regress, or leave something
//! half-finished?") composed from the same diff context `/review` uses, injected
//! back into the conversation so the agent can *fix* what it finds before
//! handing control back. It is effectively mandatory inside autonomous runs,
//! where no human eyeballs each change — hence the `Auto` default ties it to the
//! autonomy level.

use crate::tool::ApprovalLevel;

/// How the self-review pass is gated. `Auto` (the default) ties it to the
/// autonomy level — on at `balanced` (the default level) and higher, off only at
/// the tight-control levels (`ask`/`conservative`) where the human gates each
/// action anyway. The explicit modes override that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
pub(crate) enum SelfReviewMode {
    /// On at `balanced` and higher, off at `ask`/`conservative`. The default.
    #[default]
    Auto,
    /// Never self-review.
    Off,
    /// Prompt before each self-review pass.
    Ask,
    /// Self-review every diff that meets the minimum review threshold.
    On,
}

/// Whether a fired self-review covered an exact path set or had to inspect the
/// full baseline diff because at least one mutation could not be attributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelfReviewScope {
    Scoped,
    Unscoped,
}

impl SelfReviewScope {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::Unscoped => "unscoped",
        }
    }

    pub(crate) fn from_label(value: &str) -> Option<Self> {
        match value {
            "scoped" => Some(Self::Scoped),
            "unscoped" => Some(Self::Unscoped),
            _ => None,
        }
    }
}

/// What the parent did after receiving a reviewer critique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelfReviewDisposition {
    Fixed,
    NoneNeeded,
    Rebutted,
}

impl SelfReviewDisposition {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::NoneNeeded => "none_needed",
            Self::Rebutted => "rebutted",
        }
    }

    pub(crate) fn from_label(value: &str) -> Option<Self> {
        match value {
            "fixed" => Some(Self::Fixed),
            "none_needed" => Some(Self::NoneNeeded),
            "rebutted" => Some(Self::Rebutted),
            _ => None,
        }
    }
}

/// Lifecycle outcome of the reviewer lane itself. This is separate from
/// [`SelfReviewDisposition`], which records what the parent did with a critique.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelfReviewRunStatus {
    Running,
    /// Compatible default for records serialized before lifecycle status was
    /// persisted; those records represented completed review passes.
    #[default]
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    ParentInterrupted,
}

impl SelfReviewRunStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::ParentInterrupted => "parent_interrupted",
        }
    }

    pub(crate) fn from_label(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "timed_out" => Some(Self::TimedOut),
            "cancelled" => Some(Self::Cancelled),
            "parent_interrupted" => Some(Self::ParentInterrupted),
            _ => None,
        }
    }

    pub(crate) const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Structured severity counts extracted from one reviewer conclusion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct SelfReviewFindingCounts {
    pub(crate) blocker: u32,
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) nit: u32,
}

impl SelfReviewFindingCounts {
    pub(crate) fn parse(critique: &str) -> Self {
        let normalized = critique.trim().to_ascii_lowercase();
        if !normalized.contains('\n')
            && [
                "no findings",
                "no issues",
                "nothing to fix",
                "no changes needed",
                "looks correct",
                "look correct",
            ]
            .iter()
            .any(|phrase| normalized.contains(phrase))
        {
            return Self::default();
        }
        let mut counts = Self::default();
        for token in critique.split(|ch: char| !ch.is_ascii_alphanumeric()) {
            if token.eq_ignore_ascii_case("blocker") {
                counts.blocker = counts.blocker.saturating_add(1);
            } else if token.eq_ignore_ascii_case("major") {
                counts.major = counts.major.saturating_add(1);
            } else if token.eq_ignore_ascii_case("minor") {
                counts.minor = counts.minor.saturating_add(1);
            } else if token.eq_ignore_ascii_case("nit") {
                counts.nit = counts.nit.saturating_add(1);
            }
        }
        counts
    }

    pub(crate) const fn total(self) -> u32 {
        self.blocker
            .saturating_add(self.major)
            .saturating_add(self.minor)
            .saturating_add(self.nit)
    }
}

/// Durable evidence for one completion-blocking self-review pass.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct SelfReviewRunRecord {
    /// Synthetic `agent` tool call used by the parent transcript. Legacy and
    /// in-conversation fallback records have no call id.
    #[serde(default)]
    pub(crate) tool_call_id: Option<String>,
    pub(crate) started_at_ms: i64,
    pub(crate) mode: SelfReviewMode,
    pub(crate) scope: SelfReviewScope,
    pub(crate) diff_line_count: u32,
    pub(crate) reviewer_duration_ms: u64,
    pub(crate) reviewer_prompt_tokens: u64,
    pub(crate) reviewer_completion_tokens: u64,
    pub(crate) reviewer_cost_micros: Option<u64>,
    #[serde(default)]
    pub(crate) status: SelfReviewRunStatus,
    /// Bounded terminal reviewer payload, identical to the synthetic tool
    /// result when [`Self::tool_call_id`] is present.
    #[serde(default)]
    pub(crate) result: Option<String>,
    pub(crate) findings: SelfReviewFindingCounts,
    pub(crate) disposition: Option<SelfReviewDisposition>,
}

/// Terminalize lifecycle rows that were snapshotted while a process stopped.
/// A resumed session cannot still own the reviewer future, so retaining
/// `running` would permanently block completion and render an immortal spinner.
pub(crate) fn reconcile_abandoned_runs(runs: &mut [SelfReviewRunRecord]) {
    for run in runs {
        if run.status != SelfReviewRunStatus::Running {
            continue;
        }
        run.status = SelfReviewRunStatus::ParentInterrupted;
        run.result = Some(
            "Reviewer subagent interrupted before its terminal outcome was persisted.".to_string(),
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn resume_terminalizes_only_abandoned_reviewer_runs() {
        let mut running = sample_run(SelfReviewRunStatus::Running, None);
        let succeeded = sample_run(
            SelfReviewRunStatus::Succeeded,
            Some("No findings.".to_string()),
        );
        let mut runs = vec![running.clone(), succeeded.clone()];

        reconcile_abandoned_runs(&mut runs);

        running.status = SelfReviewRunStatus::ParentInterrupted;
        running.result = Some(
            "Reviewer subagent interrupted before its terminal outcome was persisted.".to_string(),
        );
        assert_eq!(runs, vec![running, succeeded]);
    }

    fn sample_run(status: SelfReviewRunStatus, result: Option<String>) -> SelfReviewRunRecord {
        SelfReviewRunRecord {
            tool_call_id: Some("self-review-test".to_string()),
            started_at_ms: 1,
            mode: SelfReviewMode::On,
            scope: SelfReviewScope::Scoped,
            diff_line_count: 1,
            reviewer_duration_ms: 0,
            reviewer_prompt_tokens: 0,
            reviewer_completion_tokens: 0,
            reviewer_cost_micros: Some(0),
            status,
            result,
            findings: SelfReviewFindingCounts::default(),
            disposition: None,
        }
    }
}

/// Lifetime self-review effectiveness rollup shown by `/perf`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelfReviewStats {
    pub runs: i64,
    pub runs_with_findings: i64,
    pub fixed: i64,
    pub none_needed: i64,
    pub rebutted: i64,
    pub findings: i64,
    pub reviewer_duration_ms: i64,
    pub reviewer_cost_micros: i64,
}

/// What to do at a would-be completion, after resolving the mode against the
/// current autonomy level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelfReviewDecision {
    /// Finish without a self-review pass.
    Skip,
    /// Run the self-review pass.
    Run,
    /// Ask the user whether to run it (degrades to `Skip` when noninteractive).
    Ask,
}

impl SelfReviewMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Ask => "ask",
            Self::On => "on",
        }
    }

    /// Parse a mode, accepting `default`→`auto` and a few yes/no aliases.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "auto" | "default" => Some(Self::Auto),
            "off" | "no" | "false" => Some(Self::Off),
            "ask" => Some(Self::Ask),
            "on" | "yes" | "true" => Some(Self::On),
            _ => None,
        }
    }

    /// Read an initial mode from the environment (`BONSAI_SELF_REVIEW`), used as
    /// the startup default when set.
    pub(crate) fn from_env() -> Option<Self> {
        std::env::var("BONSAI_SELF_REVIEW")
            .ok()
            .and_then(|value| Self::parse(&value))
    }

    /// Resolve against the autonomy level into a concrete action.
    pub(crate) fn decision(self, level: ApprovalLevel) -> SelfReviewDecision {
        match self {
            Self::Off => SelfReviewDecision::Skip,
            Self::On => SelfReviewDecision::Run,
            Self::Ask => SelfReviewDecision::Ask,
            Self::Auto => {
                // Runs by default (`balanced`) and above, so an implementation
                // gets a fresh-eyes reviewer pass without an opt-in; skipped only
                // at the tight-control levels (`ask`/`conservative`) where the
                // human is already gating each action and the pass is redundant.
                if level >= ApprovalLevel::Balanced {
                    SelfReviewDecision::Run
                } else {
                    SelfReviewDecision::Skip
                }
            }
        }
    }

    /// Human-readable status line, spelling out what `auto` resolves to right now.
    pub(crate) fn describe(self, level: ApprovalLevel) -> String {
        match self {
            Self::Auto => {
                let resolved = match self.decision(level) {
                    SelfReviewDecision::Skip => "off",
                    _ => "on",
                };
                format!("auto ({resolved} at the current autonomy level)")
            }
            other => other.label().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes_and_aliases() {
        assert_eq!(SelfReviewMode::parse("auto"), Some(SelfReviewMode::Auto));
        assert_eq!(SelfReviewMode::parse("default"), Some(SelfReviewMode::Auto));
        assert_eq!(SelfReviewMode::parse("OFF"), Some(SelfReviewMode::Off));
        assert_eq!(SelfReviewMode::parse(" on "), Some(SelfReviewMode::On));
        assert_eq!(SelfReviewMode::parse("ask"), Some(SelfReviewMode::Ask));
        assert_eq!(SelfReviewMode::parse("bogus"), None);
    }

    #[test]
    fn auto_follows_autonomy_level() {
        // Off only at the tight-control levels where the human gates each action.
        assert_eq!(
            SelfReviewMode::Auto.decision(ApprovalLevel::Ask),
            SelfReviewDecision::Skip
        );
        assert_eq!(
            SelfReviewMode::Auto.decision(ApprovalLevel::Conservative),
            SelfReviewDecision::Skip
        );
        // On from the default level upward.
        assert_eq!(
            SelfReviewMode::Auto.decision(ApprovalLevel::Balanced),
            SelfReviewDecision::Run
        );
        assert_eq!(
            SelfReviewMode::Auto.decision(ApprovalLevel::AutoAccept),
            SelfReviewDecision::Run
        );
        assert_eq!(
            SelfReviewMode::Auto.decision(ApprovalLevel::Yolo),
            SelfReviewDecision::Run
        );
    }

    #[test]
    fn explicit_modes_ignore_level() {
        assert_eq!(
            SelfReviewMode::On.decision(ApprovalLevel::Ask),
            SelfReviewDecision::Run
        );
        assert_eq!(
            SelfReviewMode::Off.decision(ApprovalLevel::Yolo),
            SelfReviewDecision::Skip
        );
        assert_eq!(
            SelfReviewMode::Ask.decision(ApprovalLevel::Balanced),
            SelfReviewDecision::Ask
        );
    }

    #[test]
    fn describe_spells_out_auto() {
        assert_eq!(
            SelfReviewMode::Auto.describe(ApprovalLevel::Conservative),
            "auto (off at the current autonomy level)"
        );
        assert_eq!(
            SelfReviewMode::Auto.describe(ApprovalLevel::Balanced),
            "auto (on at the current autonomy level)"
        );
        assert_eq!(SelfReviewMode::On.describe(ApprovalLevel::Ask), "on");
    }

    #[test]
    fn severity_parser_is_case_insensitive_and_uses_word_boundaries() {
        let counts = SelfReviewFindingCounts::parse(
            "Major: broken edge\nminor — cleanup\nNIT: naming\nBLOCKER: unsafe\nmajority",
        );
        assert_eq!(
            counts,
            SelfReviewFindingCounts {
                blocker: 1,
                major: 1,
                minor: 1,
                nit: 1,
            }
        );
        assert_eq!(counts.total(), 4);
        assert_eq!(
            SelfReviewFindingCounts::parse("No findings: no Blocker, Major, Minor, or Nit issues."),
            SelfReviewFindingCounts::default()
        );
    }
}
