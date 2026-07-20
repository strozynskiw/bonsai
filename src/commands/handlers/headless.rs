use super::super::*;

pub(in crate::commands) fn unsupported_tui_feature(message: &'static str) -> CommandMessage {
    status(message.to_string())
}
