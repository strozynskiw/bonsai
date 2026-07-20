//! Persisted first-run setup checkpoints and step selection.

/// Durable checkpoints that cannot be derived from provider or workspace state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstRunCheckpoint {
    Started,
    ModelConfirmed,
    SandboxReviewed,
    AutonomyReviewed,
    Completed,
}

impl FirstRunCheckpoint {
    pub(crate) const fn preference_key(self) -> &'static str {
        match self {
            Self::Started => "first_run_started",
            Self::ModelConfirmed => "first_run_model_confirmed",
            Self::SandboxReviewed => "first_run_sandbox_reviewed",
            Self::AutonomyReviewed => "first_run_autonomy_reviewed",
            Self::Completed => "first_run_completed",
        }
    }
}

/// Persisted checkpoint snapshot for the first-run wizard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FirstRunProgress {
    pub(crate) started: bool,
    pub(crate) model_confirmed: bool,
    pub(crate) sandbox_reviewed: bool,
    pub(crate) autonomy_reviewed: bool,
    pub(crate) completed: bool,
}

/// Ordered first-run wizard steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstRunStep {
    CredentialStorage,
    Provider,
    Model,
    WorkspaceTrust,
    Sandbox,
    Autonomy,
    FirstPrompt,
}

impl FirstRunStep {
    pub(crate) const TOTAL: usize = 7;

    pub(crate) const fn choice_count(self) -> usize {
        match self {
            Self::CredentialStorage => crate::session::CredentialPersistence::ALL.len(),
            Self::WorkspaceTrust => 2,
            Self::Sandbox => SANDBOX_CHOICES,
            Self::Autonomy => crate::tool::ApprovalLevel::ALL.len(),
            Self::Provider | Self::Model | Self::FirstPrompt => 1,
        }
    }

    /// Where the cursor starts when the step opens: the recommended choice,
    /// so Enter-through-the-wizard lands on a sensible setup.
    pub(crate) fn default_choice(self) -> usize {
        match self {
            Self::Autonomy => crate::tool::ApprovalLevel::ALL
                .iter()
                .position(|level| *level == crate::tool::ApprovalLevel::default())
                .unwrap_or(0),
            _ => 0,
        }
    }

    pub(crate) const fn number(self) -> usize {
        match self {
            Self::CredentialStorage => 1,
            Self::Provider => 2,
            Self::Model => 3,
            Self::WorkspaceTrust => 4,
            Self::Sandbox => 5,
            Self::Autonomy => 6,
            Self::FirstPrompt => 7,
        }
    }
}

/// Sandbox postures offered by the wizard, in cursor order: confined with the
/// network blocked (recommended), confined with network, or no confinement.
pub(crate) const SANDBOX_CHOICES: usize = 3;

/// Decide whether a user needs the wizard and where a resumed wizard continues.
/// Existing installations that chose credential storage before this wizard
/// shipped are treated as complete unless the wizard was explicitly started.
pub(crate) fn initial_step(
    progress: FirstRunProgress,
    has_credential_preference: bool,
    has_authorized_provider: bool,
    workspace_trust_needs_decision: bool,
) -> Option<FirstRunStep> {
    if progress.completed || (!progress.started && has_credential_preference) {
        return None;
    }
    if !has_credential_preference {
        return Some(FirstRunStep::CredentialStorage);
    }
    if !has_authorized_provider {
        return Some(FirstRunStep::Provider);
    }
    if !progress.model_confirmed {
        return Some(FirstRunStep::Model);
    }
    if workspace_trust_needs_decision {
        return Some(FirstRunStep::WorkspaceTrust);
    }
    if !progress.sandbox_reviewed {
        return Some(FirstRunStep::Sandbox);
    }
    if !progress.autonomy_reviewed {
        return Some(FirstRunStep::Autonomy);
    }
    Some(FirstRunStep::FirstPrompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_install_walks_each_persisted_checkpoint() {
        let mut progress = FirstRunProgress::default();
        assert_eq!(
            initial_step(progress, false, false, true),
            Some(FirstRunStep::CredentialStorage)
        );

        progress.started = true;
        assert_eq!(
            initial_step(progress, true, false, true),
            Some(FirstRunStep::Provider)
        );
        assert_eq!(
            initial_step(progress, true, true, true),
            Some(FirstRunStep::Model)
        );

        progress.model_confirmed = true;
        assert_eq!(
            initial_step(progress, true, true, true),
            Some(FirstRunStep::WorkspaceTrust)
        );
        assert_eq!(
            initial_step(progress, true, true, false),
            Some(FirstRunStep::Sandbox)
        );

        progress.sandbox_reviewed = true;
        assert_eq!(
            initial_step(progress, true, true, false),
            Some(FirstRunStep::Autonomy)
        );

        progress.autonomy_reviewed = true;
        assert_eq!(
            initial_step(progress, true, true, false),
            Some(FirstRunStep::FirstPrompt)
        );
        progress.completed = true;
        assert_eq!(initial_step(progress, true, true, false), None);
    }

    #[test]
    fn existing_installations_are_not_forced_into_the_new_wizard() {
        assert_eq!(
            initial_step(FirstRunProgress::default(), true, false, true),
            None
        );
    }
}
