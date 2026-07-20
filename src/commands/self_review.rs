//! `/self-review` — gate the self-review-before-done pass. `auto` (the
//! default) follows the autonomy level (on at `auto-accept`+); `on`/`ask`/`off`
//! override it.

use crate::self_review::SelfReviewMode;
use crate::tool::ApprovalLevel;

pub(crate) const SELF_REVIEW_USAGE: &str =
    "Usage: /self-review [auto|on|ask|off|status | model | model default]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelfReviewCommandRequest {
    Set(SelfReviewMode),
    Status,
    /// Open the model picker to pin the self-review lane's provider/model/effort.
    OpenModelPicker,
    /// Clear the pinned model so the lane falls back to the parent model.
    ClearModel,
}

pub(crate) fn parse_self_review_command(
    input: &str,
) -> std::result::Result<SelfReviewCommandRequest, String> {
    let args = super::command_args(input, "/self-review", SELF_REVIEW_USAGE)?;
    match args.as_slice() {
        [] | ["status"] => Ok(SelfReviewCommandRequest::Status),
        ["model"] => Ok(SelfReviewCommandRequest::OpenModelPicker),
        // `default`/`off`/`clear` all reset to the parent model.
        ["model", "default" | "off" | "clear"] => Ok(SelfReviewCommandRequest::ClearModel),
        [mode] => SelfReviewMode::parse(mode)
            .map(SelfReviewCommandRequest::Set)
            .ok_or_else(|| SELF_REVIEW_USAGE.to_string()),
        _ => Err(SELF_REVIEW_USAGE.to_string()),
    }
}

pub(crate) fn self_review_set_message(mode: SelfReviewMode, level: ApprovalLevel) -> String {
    format!("Self-review set to {}.", mode.describe(level))
}

pub(crate) fn self_review_status_message(mode: SelfReviewMode, level: ApprovalLevel) -> String {
    format!("Self-review is {}.", mode.describe(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes_status_and_default() {
        assert_eq!(
            parse_self_review_command("/self-review"),
            Ok(SelfReviewCommandRequest::Status)
        );
        assert_eq!(
            parse_self_review_command("/self-review status"),
            Ok(SelfReviewCommandRequest::Status)
        );
        assert_eq!(
            parse_self_review_command("/self-review on"),
            Ok(SelfReviewCommandRequest::Set(SelfReviewMode::On))
        );
        assert_eq!(
            parse_self_review_command("/self-review default"),
            Ok(SelfReviewCommandRequest::Set(SelfReviewMode::Auto))
        );
        assert!(parse_self_review_command("/self-review bogus").is_err());
        assert!(parse_self_review_command("/self-review a b").is_err());
    }

    #[test]
    fn parses_model_picker_and_clear() {
        assert_eq!(
            parse_self_review_command("/self-review model"),
            Ok(SelfReviewCommandRequest::OpenModelPicker)
        );
        for reset in ["default", "off", "clear"] {
            assert_eq!(
                parse_self_review_command(&format!("/self-review model {reset}")),
                Ok(SelfReviewCommandRequest::ClearModel)
            );
        }
        assert!(parse_self_review_command("/self-review model bogus").is_err());
    }

    #[test]
    fn status_message_spells_out_auto() {
        assert_eq!(
            self_review_status_message(SelfReviewMode::Auto, ApprovalLevel::AutoAccept),
            "Self-review is auto (on at the current autonomy level)."
        );
        assert_eq!(
            self_review_set_message(SelfReviewMode::Off, ApprovalLevel::Yolo),
            "Self-review set to off."
        );
    }
}
