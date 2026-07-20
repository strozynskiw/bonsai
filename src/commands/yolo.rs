//! `/yolo` is a one-word alias onto the [`crate::tool::ApprovalLevel`] axis:
//! `on` selects `yolo` (no guardrails), `off` returns to `ask`, and a bare
//! `/yolo` toggles between them. Both the TUI (`apply_autonomy_command`) and the
//! headless path route it into the shared approval holder; the level-naming
//! messages live in [`crate::commands::autonomy`].

const YOLO_USAGE: &str = "Usage: /yolo [on|off|status]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum YoloCommandRequest {
    Toggle,
    Set(bool),
    Status,
}

pub(crate) fn parse_yolo_command(input: &str) -> std::result::Result<YoloCommandRequest, String> {
    let args = super::command_args(input, "/yolo", YOLO_USAGE)?;
    match args.as_slice() {
        [] => Ok(YoloCommandRequest::Toggle),
        ["on"] => Ok(YoloCommandRequest::Set(true)),
        ["off"] => Ok(YoloCommandRequest::Set(false)),
        ["status"] => Ok(YoloCommandRequest::Status),
        _ => Err(YOLO_USAGE.to_string()),
    }
}
