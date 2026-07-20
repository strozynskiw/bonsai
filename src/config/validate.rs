//! Post-parse validation naming the extension + field, per entry — never a
//! hard error for the whole file.

use super::schema::{HookAction, HookDef, McpServerConfig, McpTransportConfig};

/// A dotted-ID-safe name: `^[a-z0-9][a-z0-9_-]*$` — no dots, so a config-supplied
/// name can never forge a namespace separator (`mcp.github`, `hook.cargo-fmt`).
/// Lives in `config` because names are validated at config parse time; the
/// extension runtime consumes only already-validated names.
pub(crate) fn validate_extension_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("name must not be empty".to_string());
    };
    let first_ok = first.is_ascii_lowercase() || first.is_ascii_digit();
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if first_ok && rest_ok {
        Ok(())
    } else {
        Err(format!(
            "'{name}' is not a valid name: expected ^[a-z0-9][a-z0-9_-]*$"
        ))
    }
}

/// Field-level checks a bare serde deserialize can't express (a present but
/// empty required string).
pub(crate) fn validate_mcp_server(config: &McpServerConfig) -> Result<(), String> {
    match &config.transport {
        McpTransportConfig::Stdio { command, .. } if command.trim().is_empty() => {
            Err("`command` must not be empty for a stdio transport".to_string())
        }
        McpTransportConfig::Http { url, .. } if url.trim().is_empty() => {
            Err("`url` must not be empty for an http transport".to_string())
        }
        McpTransportConfig::Http {
            oauth_client_id: Some(client_id),
            ..
        } if client_id.trim().is_empty() => {
            Err("`oauth_client_id` must not be empty when present".to_string())
        }
        McpTransportConfig::Http { oauth_scopes, .. }
            if oauth_scopes.iter().any(|scope| scope.trim().is_empty()) =>
        {
            Err("`oauth_scopes` must not contain empty values".to_string())
        }
        _ => Ok(()),
    }
}

/// A hook's `name` follows the same namespace rule as an extension name (it
/// ends up in `hook.<name>` permission-rule patterns), and its action must
/// carry non-empty content — a present-but-empty `command`/`url`/`prompt`
/// would otherwise reach the hooks runtime as a silent no-op or, worse, an
/// always-matching empty shell command.
pub(crate) fn validate_hook(hook: &HookDef) -> Result<(), String> {
    validate_extension_name(&hook.name).map_err(|err| format!("`name`: {err}"))?;
    match &hook.action {
        HookAction::Shell { command } if command.trim().is_empty() => {
            Err("`command` must not be empty for a shell action".to_string())
        }
        HookAction::Http { url, .. } if url.trim().is_empty() => {
            Err("`url` must not be empty for an http action".to_string())
        }
        HookAction::LlmPrompt { prompt } if prompt.trim().is_empty() => {
            Err("`prompt` must not be empty for an llm_prompt action".to_string())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::schema::{FailureBehavior, HookEvent, HookMatcher};
    use super::*;

    #[test]
    fn accepts_lowercase_digits_dash_underscore() {
        assert!(validate_extension_name("github").is_ok());
        assert!(validate_extension_name("cargo-fmt").is_ok());
        assert!(validate_extension_name("server_2").is_ok());
        assert!(validate_extension_name("7zip").is_ok());
    }

    #[test]
    fn rejects_empty_uppercase_dots() {
        assert!(validate_extension_name("").is_err());
        assert!(validate_extension_name("GitHub").is_err());
        assert!(validate_extension_name("mcp.github").is_err());
        assert!(validate_extension_name("-github").is_err());
    }

    fn shell_hook(name: &str, command: &str) -> HookDef {
        HookDef {
            name: name.to_string(),
            event: HookEvent::PostFileWrite,
            matcher: HookMatcher::default(),
            action: HookAction::Shell {
                command: command.to_string(),
            },
            timeout_secs: 30,
            blocking: false,
            on_failure: FailureBehavior::Warn,
            capabilities: Vec::new(),
            enabled: true,
        }
    }

    #[test]
    fn accepts_a_well_formed_hook() {
        assert!(validate_hook(&shell_hook("cargo-fmt", "cargo fmt")).is_ok());
    }

    #[test]
    fn rejects_a_dotted_hook_name() {
        let err = validate_hook(&shell_hook("bad.name", "echo hi")).unwrap_err();
        assert!(err.contains("name"), "{err}");
    }

    #[test]
    fn rejects_an_empty_shell_command() {
        let err = validate_hook(&shell_hook("cargo-fmt", "")).unwrap_err();
        assert!(err.contains("command"), "{err}");
    }

    #[test]
    fn rejects_an_empty_http_url() {
        let mut hook = shell_hook("audit", "unused");
        hook.action = HookAction::Http {
            url: "   ".to_string(),
            headers: Default::default(),
        };
        assert!(validate_hook(&hook).is_err());
    }

    #[test]
    fn rejects_an_empty_llm_prompt() {
        let mut hook = shell_hook("review", "unused");
        hook.action = HookAction::LlmPrompt {
            prompt: "".to_string(),
        };
        assert!(validate_hook(&hook).is_err());
    }
}
