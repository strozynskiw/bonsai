//! `/autonomy` — the single command for the session's approval level: how much
//! the agent does without asking, from `ask` (prompt on every risky action) to
//! `yolo` (no guardrails). Replaces the previous `/mode` + `/auto-approve` pair;
//! `/yolo` remains as a one-word alias for the top level.

use crate::tool::ApprovalLevel;

pub(crate) const AUTONOMY_USAGE: &str =
    "Usage: /autonomy [ask|conservative|balanced|auto-accept|yolo|status]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutonomyCommandRequest {
    Set(ApprovalLevel),
    Status,
}

pub(crate) fn parse_autonomy_command(
    input: &str,
) -> std::result::Result<AutonomyCommandRequest, String> {
    let args = super::command_args(input, "/autonomy", AUTONOMY_USAGE)?;
    match args.as_slice() {
        [] | ["status"] => Ok(AutonomyCommandRequest::Status),
        [level] => ApprovalLevel::parse(level)
            .map(AutonomyCommandRequest::Set)
            .ok_or_else(|| AUTONOMY_USAGE.to_string()),
        _ => Err(AUTONOMY_USAGE.to_string()),
    }
}

pub(crate) fn autonomy_set_message(level: ApprovalLevel) -> String {
    format!("Autonomy set to {}.", level.label())
}

pub(crate) fn autonomy_status_message(level: ApprovalLevel) -> String {
    format!("Autonomy is {}.", level.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_levels_status_and_aliases() {
        assert_eq!(
            parse_autonomy_command("/autonomy"),
            Ok(AutonomyCommandRequest::Status)
        );
        assert_eq!(
            parse_autonomy_command("/autonomy status"),
            Ok(AutonomyCommandRequest::Status)
        );
        assert_eq!(
            parse_autonomy_command("/autonomy auto-accept"),
            Ok(AutonomyCommandRequest::Set(ApprovalLevel::AutoAccept))
        );
        // input aliases carried over for muscle memory
        assert_eq!(
            parse_autonomy_command("/autonomy default"),
            Ok(AutonomyCommandRequest::Set(ApprovalLevel::Ask))
        );
        assert_eq!(
            parse_autonomy_command("/autonomy yolo"),
            Ok(AutonomyCommandRequest::Set(ApprovalLevel::Yolo))
        );
        assert!(parse_autonomy_command("/autonomy bogus").is_err());
        assert!(parse_autonomy_command("/autonomy a b").is_err());
    }

    #[test]
    fn messages_name_the_level() {
        assert_eq!(
            autonomy_set_message(ApprovalLevel::Balanced),
            "Autonomy set to balanced."
        );
    }
}
