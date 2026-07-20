use crate::model_role::ModelShortcutKey;

const RETRY_USAGE: &str = "Usage: /retry [<model-shortcut>]";

/// Parsed request for retrying the latest human turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryCommandRequest {
    pub(crate) model_shortcut: Option<ModelShortcutKey>,
}

pub(crate) fn parse_retry_command(input: &str) -> Result<RetryCommandRequest, String> {
    let args = super::command_args(input, "/retry", RETRY_USAGE)?;
    match args.as_slice() {
        [] => Ok(RetryCommandRequest {
            model_shortcut: None,
        }),
        [shortcut] => shortcut
            .parse::<ModelShortcutKey>()
            .map(|model_shortcut| RetryCommandRequest {
                model_shortcut: Some(model_shortcut),
            })
            .map_err(|_| RETRY_USAGE.to_string()),
        _ => Err(RETRY_USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_and_shortcut_retries() {
        assert_eq!(
            parse_retry_command("/retry"),
            Ok(RetryCommandRequest {
                model_shortcut: None
            })
        );
        assert_eq!(
            parse_retry_command("/retry F"),
            Ok(RetryCommandRequest {
                model_shortcut: ModelShortcutKey::new('f')
            })
        );
    }

    #[test]
    fn rejects_invalid_retry_arguments() {
        assert_eq!(parse_retry_command("/retry fast"), Err(RETRY_USAGE.into()));
        assert_eq!(parse_retry_command("/retry f s"), Err(RETRY_USAGE.into()));
        assert_eq!(parse_retry_command("/again"), Err(RETRY_USAGE.into()));
    }
}
