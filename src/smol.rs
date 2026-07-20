//! Explicit small-model profile preference and effective-state resolution.

/// User preference for the small-model profile.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SmolPreference {
    /// Always use the standard coding profile.
    #[default]
    Off,
    /// Always use the reduced SMOL coding profile.
    On,
}

impl SmolPreference {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 2] = [Self::Off, Self::On];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            _ => None,
        }
    }

    pub(crate) const fn is_effective(self) -> bool {
        match self {
            Self::Off => false,
            Self::On => true,
        }
    }
}

/// Configured and effective SMOL state for the current model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SmolProfile {
    pub(crate) preference: SmolPreference,
    pub(crate) enabled: bool,
    pub(crate) context_window_tokens: usize,
}

impl SmolProfile {
    pub(crate) const fn resolve(preference: SmolPreference, context_window_tokens: usize) -> Self {
        Self {
            preference,
            enabled: preference.is_effective(),
            context_window_tokens,
        }
    }

    pub(crate) const fn effective_label(self) -> &'static str {
        if self.enabled { "on" } else { "off" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_preference_is_independent_of_context_window() {
        assert!(SmolPreference::On.is_effective());
        assert!(!SmolPreference::Off.is_effective());
        assert!(SmolProfile::resolve(SmolPreference::On, 1_000_000).enabled);
        assert!(!SmolProfile::resolve(SmolPreference::Off, 8_192).enabled);
    }

    #[test]
    fn preference_round_trips_through_stable_labels() {
        for preference in SmolPreference::ALL {
            assert_eq!(SmolPreference::parse(preference.as_str()), Some(preference));
        }
        assert_eq!(SmolPreference::parse("auto"), None);
        assert_eq!(SmolPreference::parse("unknown"), None);
    }
}
