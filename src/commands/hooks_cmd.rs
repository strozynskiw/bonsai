//! `/hooks` — status, session control, and on-demand testing for configured
//! hooks.

use crate::extension::ExtensionId;
use crate::extension::status::ExtensionRegistry;
use crate::hooks::{HookDecision, HookEngine};

pub(crate) const HOOKS_USAGE: &str = "Usage: /hooks [enable|disable|test <name>]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HooksCommandRequest {
    View,
    Enable(String),
    Disable(String),
    Test(String),
}

pub(crate) fn parse_hooks_command(input: &str) -> Result<HooksCommandRequest, String> {
    let args = super::command_args(input, "/hooks", HOOKS_USAGE)?;
    match args.as_slice() {
        [] => Ok(HooksCommandRequest::View),
        ["enable", name] => Ok(HooksCommandRequest::Enable((*name).to_string())),
        ["disable", name] => Ok(HooksCommandRequest::Disable((*name).to_string())),
        ["test", name] => Ok(HooksCommandRequest::Test((*name).to_string())),
        _ => Err(HOOKS_USAGE.to_string()),
    }
}

/// One line per configured hook: name, event, matcher, action type, state,
/// provenance.
pub(crate) fn format_hooks_view(extensions: &ExtensionRegistry) -> String {
    let hooks: Vec<_> = extensions
        .snapshot()
        .into_iter()
        .filter(|status| matches!(status.id, ExtensionId::Hook(_)))
        .collect();
    if hooks.is_empty() {
        return "Hooks: none configured.".to_string();
    }
    let mut lines = vec!["Hooks:".to_string()];
    for status in hooks {
        let ExtensionId::Hook(name) = &status.id else {
            continue;
        };
        let state = status.state.label();
        lines.push(format!(
            "  {name} — {state}, {} ({})",
            status.detail,
            status.source.label()
        ));
    }
    lines.join("\n")
}

fn format_decision(decision: &HookDecision) -> String {
    match decision {
        HookDecision::Continue => "continue".to_string(),
        HookDecision::Block { reason } => format!("block: {reason}"),
        HookDecision::ModifyArgs { args } => format!("modify_args: {args}"),
        HookDecision::AddContext { text } => format!("add_context: {text}"),
    }
}

pub(crate) async fn apply_hooks_command(
    request: HooksCommandRequest,
    hooks: &HookEngine,
    extensions: &ExtensionRegistry,
) -> Result<String, String> {
    match request {
        HooksCommandRequest::View => Ok(format_hooks_view(extensions)),
        HooksCommandRequest::Enable(name) => {
            if hooks.set_enabled(&name, true) {
                Ok(format!(
                    "Enabled '{name}' for this session. To persist, set enabled = true in its [[hooks]] entry."
                ))
            } else {
                Err(unknown_hook_message(&name, hooks))
            }
        }
        HooksCommandRequest::Disable(name) => {
            if hooks.set_enabled(&name, false) {
                Ok(format!(
                    "Disabled '{name}' for this session. To persist, set enabled = false in its [[hooks]] entry."
                ))
            } else {
                Err(unknown_hook_message(&name, hooks))
            }
        }
        HooksCommandRequest::Test(name) => match hooks.test_fire(&name).await {
            Some(outcome) => {
                let mut lines = vec![format!(
                    "'{name}' fired: {}",
                    format_decision(&outcome.decision)
                )];
                lines.extend(outcome.warnings.iter().map(|w| format!("  warning: {w}")));
                Ok(lines.join("\n"))
            }
            None => Err(unknown_hook_message(&name, hooks)),
        },
    }
}

fn unknown_hook_message(name: &str, hooks: &HookEngine) -> String {
    let known = hooks.hook_names().join(", ");
    if known.is_empty() {
        format!("Unknown hook '{name}'. No hooks are configured.")
    } else {
        format!("Unknown hook '{name}'. Configured: {known}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_view_toggle_and_test_variants() {
        assert_eq!(parse_hooks_command("/hooks"), Ok(HooksCommandRequest::View));
        assert_eq!(
            parse_hooks_command("/hooks enable cargo-fmt"),
            Ok(HooksCommandRequest::Enable("cargo-fmt".to_string()))
        );
        assert_eq!(
            parse_hooks_command("/hooks disable cargo-fmt"),
            Ok(HooksCommandRequest::Disable("cargo-fmt".to_string()))
        );
        assert_eq!(
            parse_hooks_command("/hooks test cargo-fmt"),
            Ok(HooksCommandRequest::Test("cargo-fmt".to_string()))
        );
        assert!(parse_hooks_command("/hooks bogus").is_err());
        assert!(parse_hooks_command("/hooks enable").is_err());
    }

    #[test]
    fn view_reports_no_hooks() {
        let extensions = ExtensionRegistry::new();
        assert_eq!(format_hooks_view(&extensions), "Hooks: none configured.");
    }

    #[tokio::test]
    async fn enable_unknown_hook_reports_unknown() {
        let extensions = ExtensionRegistry::new();
        let hooks = HookEngine::disabled();
        let result = apply_hooks_command(
            HooksCommandRequest::Enable("ghost".to_string()),
            &hooks,
            &extensions,
        )
        .await;
        assert_eq!(
            result,
            Err("Unknown hook 'ghost'. No hooks are configured.".to_string())
        );
    }

    #[tokio::test]
    async fn test_unknown_hook_reports_unknown() {
        let extensions = ExtensionRegistry::new();
        let hooks = HookEngine::disabled();
        let result = apply_hooks_command(
            HooksCommandRequest::Test("ghost".to_string()),
            &hooks,
            &extensions,
        )
        .await;
        assert!(result.is_err());
    }
}
