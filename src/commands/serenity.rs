//! `/serenity` — TUI-only calm presentation mode: hide thinking and fold tool
//! groups by default without changing the model/provider reasoning setting.

pub(crate) const SERENITY_USAGE: &str = "Usage: /serenity [on|off|status]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SerenityCommandRequest {
    /// Bare `/serenity` flips the current state.
    Toggle,
    Set(bool),
    Status,
}

impl SerenityCommandRequest {
    /// The serenity-mode state this request selects given the `current` state.
    pub(crate) fn target_state(self, current: bool) -> Option<bool> {
        match self {
            SerenityCommandRequest::Set(on) => Some(on),
            SerenityCommandRequest::Toggle => Some(!current),
            SerenityCommandRequest::Status => None,
        }
    }
}

pub(crate) fn parse_serenity_command(input: &str) -> Result<SerenityCommandRequest, String> {
    let args = super::command_args(input, "/serenity", SERENITY_USAGE)?;
    match args.as_slice() {
        [] => Ok(SerenityCommandRequest::Toggle),
        ["status"] => Ok(SerenityCommandRequest::Status),
        ["on"] => Ok(SerenityCommandRequest::Set(true)),
        ["off"] => Ok(SerenityCommandRequest::Set(false)),
        _ => Err(SERENITY_USAGE.to_string()),
    }
}

pub(crate) fn serenity_set_message(on: bool) -> &'static str {
    if on {
        "Serenity mode enabled: thinking hidden, tool groups folded."
    } else {
        "Serenity mode disabled."
    }
}

pub(crate) fn serenity_status_message(on: bool) -> &'static str {
    if on {
        "Serenity mode is on: thinking hidden, tool groups folded."
    } else {
        "Serenity mode is off."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_toggles() {
        assert_eq!(
            parse_serenity_command("/serenity"),
            Ok(SerenityCommandRequest::Toggle)
        );
        assert_eq!(
            parse_serenity_command("/serenity status"),
            Ok(SerenityCommandRequest::Status)
        );
        assert_eq!(
            parse_serenity_command("/serenity on"),
            Ok(SerenityCommandRequest::Set(true))
        );
        assert_eq!(
            parse_serenity_command("/serenity off"),
            Ok(SerenityCommandRequest::Set(false))
        );
        assert_eq!(
            parse_serenity_command("/serenity maybe"),
            Err(SERENITY_USAGE.to_string())
        );
    }

    #[test]
    fn target_state_resolves_toggle_against_current() {
        assert_eq!(
            SerenityCommandRequest::Toggle.target_state(false),
            Some(true)
        );
        assert_eq!(
            SerenityCommandRequest::Toggle.target_state(true),
            Some(false)
        );
        assert_eq!(
            SerenityCommandRequest::Set(true).target_state(true),
            Some(true)
        );
        assert_eq!(
            SerenityCommandRequest::Set(false).target_state(false),
            Some(false)
        );
        assert_eq!(SerenityCommandRequest::Status.target_state(true), None);
    }
}
