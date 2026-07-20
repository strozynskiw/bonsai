//! `/smol` — small-model profile: minimal tools, compact prompt/context, and
//! aggressive model-facing context trimming.

use crate::smol::{SmolPreference, SmolProfile};

pub(crate) const SMOL_USAGE: &str = "Usage: /smol [on|off|status]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SmolCommandRequest {
    /// Bare `/smol` creates an explicit override opposite the effective state.
    Toggle,
    Set(SmolPreference),
    Status,
}

impl SmolCommandRequest {
    /// The preference selected from the current effective profile. Status only
    /// reports and therefore returns `None`.
    pub(crate) fn target_preference(self, current: SmolProfile) -> Option<SmolPreference> {
        match self {
            SmolCommandRequest::Set(preference) => Some(preference),
            SmolCommandRequest::Toggle => Some(if current.enabled {
                SmolPreference::Off
            } else {
                SmolPreference::On
            }),
            SmolCommandRequest::Status => None,
        }
    }
}

pub(crate) fn parse_smol_command(input: &str) -> Result<SmolCommandRequest, String> {
    let args = super::command_args(input, "/smol", SMOL_USAGE)?;
    match args.as_slice() {
        [] => Ok(SmolCommandRequest::Toggle),
        ["status"] => Ok(SmolCommandRequest::Status),
        ["on"] => Ok(SmolCommandRequest::Set(SmolPreference::On)),
        ["off"] => Ok(SmolCommandRequest::Set(SmolPreference::Off)),
        _ => Err(SMOL_USAGE.to_string()),
    }
}

pub(crate) fn smol_set_message(profile: SmolProfile) -> String {
    format!(
        "SMOL preference set to {}; effective profile: {} ({} context).",
        profile.preference.as_str(),
        profile.effective_label(),
        crate::util::format::format_tokens_k(profile.context_window_tokens)
    )
}

pub(crate) fn smol_status_message(profile: SmolProfile) -> String {
    format!(
        "SMOL preference: {}; effective profile: {} ({} context).",
        profile.preference.as_str(),
        profile.effective_label(),
        crate::util::format::format_tokens_k(profile.context_window_tokens)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_toggles() {
        // Bare `/smol` toggles; explicit `status` reports without changing state.
        assert_eq!(parse_smol_command("/smol"), Ok(SmolCommandRequest::Toggle));
        assert_eq!(
            parse_smol_command("/smol status"),
            Ok(SmolCommandRequest::Status)
        );
        assert_eq!(
            parse_smol_command("/smol on"),
            Ok(SmolCommandRequest::Set(SmolPreference::On))
        );
        assert_eq!(
            parse_smol_command("/smol off"),
            Ok(SmolCommandRequest::Set(SmolPreference::Off))
        );
        assert_eq!(
            parse_smol_command("/smol auto"),
            Err(SMOL_USAGE.to_string())
        );
        assert_eq!(
            parse_smol_command("/smol maybe"),
            Err(SMOL_USAGE.to_string())
        );
    }

    #[test]
    fn target_preference_resolves_toggle_against_effective_state() {
        let off = SmolProfile::resolve(SmolPreference::Off, 128_000);
        let on = SmolProfile::resolve(SmolPreference::On, 8_192);
        assert_eq!(
            SmolCommandRequest::Toggle.target_preference(off),
            Some(SmolPreference::On)
        );
        assert_eq!(
            SmolCommandRequest::Toggle.target_preference(on),
            Some(SmolPreference::Off)
        );
        assert_eq!(SmolCommandRequest::Status.target_preference(on), None);
    }

    #[test]
    fn status_reports_configured_and_effective_profile() {
        let profile = SmolProfile::resolve(SmolPreference::Off, 8_192);
        assert_eq!(
            smol_status_message(profile),
            "SMOL preference: off; effective profile: off (8.2k context)."
        );
    }
}
