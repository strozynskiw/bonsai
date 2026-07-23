//! `/pure` — ultra-minimal mode: empty tools, slim prompt, no context.

pub(crate) const PURE_USAGE: &str = "Usage: /pure [on|off|status]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PureCommandRequest {
    /// Bare `/pure` toggles.
    Toggle,
    Set(PureTarget),
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PureTarget {
    On,
    Off,
}

pub(crate) fn parse_pure_command(input: &str) -> Result<PureCommandRequest, String> {
    let args = crate::commands::command_args(input, "/pure", PURE_USAGE)?;
    match args.as_slice() {
        [] => Ok(PureCommandRequest::Toggle),
        ["status"] => Ok(PureCommandRequest::Status),
        ["on"] => Ok(PureCommandRequest::Set(PureTarget::On)),
        ["off"] => Ok(PureCommandRequest::Set(PureTarget::Off)),
        _ => Err(PURE_USAGE.to_string()),
    }
}

pub(crate) fn pure_status_message(active: bool) -> String {
    if active {
        "Pure mode is active — zero built-in tools, slim prompt, no context.".to_string()
    } else {
        "Pure mode is off.".to_string()
    }
}

pub(crate) fn pure_set_message(target: PureTarget) -> String {
    match target {
        PureTarget::On => {
            "Pure mode on — zero built-in tools, slim prompt, no context.".to_string()
        }
        PureTarget::Off => "Pure mode off — full coding tools and context restored.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pure_commands() {
        assert_eq!(parse_pure_command("/pure"), Ok(PureCommandRequest::Toggle));
        assert_eq!(
            parse_pure_command("/pure status"),
            Ok(PureCommandRequest::Status)
        );
        assert_eq!(
            parse_pure_command("/pure on"),
            Ok(PureCommandRequest::Set(PureTarget::On))
        );
        assert_eq!(
            parse_pure_command("/pure off"),
            Ok(PureCommandRequest::Set(PureTarget::Off))
        );
        assert_eq!(
            parse_pure_command("/pure maybe"),
            Err(PURE_USAGE.to_string())
        );
    }
}
