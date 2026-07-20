//! `/config` — inspect the layered `.bonsai/config.toml` view: the
//! merged result, which layer each key came from, and any load diagnostics.

use crate::config::{
    BatchingPolicy, Config, ConfigDiagnostic, ConfigSource, FailureBehavior, HookAction, HookDef,
    McpServerConfig, McpTransportConfig,
};
use crate::extension::capabilities::capability_label;

pub(crate) const CONFIG_USAGE: &str = "Usage: /config [validate|edit [project|global]]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigEditScope {
    Project,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigCommandRequest {
    View,
    Validate,
    Edit(Option<ConfigEditScope>),
}

pub(crate) fn parse_config_command(input: &str) -> Result<ConfigCommandRequest, String> {
    let args = super::command_args(input, "/config", CONFIG_USAGE)?;
    match args.as_slice() {
        [] => Ok(ConfigCommandRequest::View),
        ["validate"] => Ok(ConfigCommandRequest::Validate),
        ["edit"] => Ok(ConfigCommandRequest::Edit(None)),
        ["edit", "project"] => Ok(ConfigCommandRequest::Edit(Some(ConfigEditScope::Project))),
        ["edit", "global"] => Ok(ConfigCommandRequest::Edit(Some(ConfigEditScope::Global))),
        _ => Err(CONFIG_USAGE.to_string()),
    }
}

/// The merged view: each section's entries annotated with their winning
/// [`ConfigSource`](crate::config::ConfigSource), plus layer paths and
/// diagnostics.
pub(crate) fn format_config_view(config: &Config) -> String {
    let mut lines = vec![format!(
        "Layers: global {} · project {} ({})",
        config.layers.global_path.display(),
        config.layers.project_path.display(),
        config.layers.project_path_source.label(),
    )];

    if config.mcp_servers.is_empty() {
        lines.push("MCP servers: none configured.".to_string());
    } else {
        lines.push("MCP servers:".to_string());
        for (name, (server, source)) in &config.mcp_servers {
            lines.push(describe_mcp_server(name, server, *source));
        }
    }

    if config.hooks.is_empty() {
        lines.push("Hooks: none configured.".to_string());
    } else {
        lines.push("Hooks:".to_string());
        for (hook, source) in &config.hooks {
            lines.push(describe_hook(hook, *source));
        }
    }

    let network = match config.sandbox.deny_network {
        Some(true) => ", network denied by config",
        Some(false) => ", network allowed by config",
        None => "",
    };
    lines.push(format!(
        "Sandbox: {} extra writable root(s){network}",
        config.sandbox.writable_roots.len(),
    ));
    lines.push(format!(
        "Verification: test={}, build={}, after_edit={}",
        configured_lane_label(config.verification.test.as_deref()),
        configured_lane_label(config.verification.build.as_deref()),
        config.verification.after_edit.label(),
    ));

    lines.push(format_config_diagnostics(&config.diagnostics));
    lines.join("\n")
}

fn configured_lane_label(commands: Option<&[String]>) -> String {
    match commands {
        None => "detected".to_string(),
        Some(commands) => format!("{} configured", commands.len()),
    }
}

fn describe_mcp_server(name: &str, server: &McpServerConfig, source: ConfigSource) -> String {
    let transport = match &server.transport {
        McpTransportConfig::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let mut detail = format!("stdio `{command}");
            for arg in args {
                detail.push(' ');
                detail.push_str(arg);
            }
            detail.push('`');
            if !env.is_empty() {
                detail.push_str(&format!(", {} env var(s)", env.len()));
            }
            if let Some(cwd) = cwd {
                detail.push_str(&format!(", cwd {}", cwd.display()));
            }
            detail
        }
        McpTransportConfig::Http {
            url,
            headers,
            oauth_client_id,
            oauth_scopes,
        } => {
            let mut detail = format!("http {url}");
            if !headers.is_empty() {
                detail.push_str(&format!(", {} header(s)", headers.len()));
            }
            if oauth_client_id.is_some() {
                detail.push_str(", OAuth client configured");
            }
            if !oauth_scopes.is_empty() {
                detail.push_str(&format!(", {} OAuth scope(s)", oauth_scopes.len()));
            }
            detail
        }
    };
    let state = if server.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let tools = if server.allow_tools.is_empty() {
        "all discovered tools".to_string()
    } else {
        format!("tools=[{}]", server.allow_tools.join(", "))
    };
    let capabilities = if server.declared.capabilities.is_empty() {
        "undeclared".to_string()
    } else {
        format!(
            "capabilities=[{}]",
            server
                .declared
                .capabilities
                .iter()
                .map(|c| capability_label(*c))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "  {name} — {transport}, {state} ({}), {tools}, {capabilities}, batching={}, timeout={}s",
        source.label(),
        batching_label(server.declared.batching),
        server.timeout_secs,
    )
}

fn describe_hook(hook: &HookDef, source: ConfigSource) -> String {
    let state = if hook.enabled { "enabled" } else { "disabled" };
    let action = match &hook.action {
        HookAction::Shell { command } => format!("shell `{command}`"),
        HookAction::Http { url, headers } if headers.is_empty() => format!("http {url}"),
        HookAction::Http { url, headers } => format!("http {url} ({} header(s))", headers.len()),
        HookAction::LlmPrompt { prompt } => format!("llm_prompt ({} char(s))", prompt.len()),
    };
    let mut matcher_parts = Vec::new();
    if let Some(tool) = &hook.matcher.tool {
        matcher_parts.push(format!("tool={tool}"));
    }
    if let Some(path) = &hook.matcher.path {
        matcher_parts.push(format!("path={path}"));
    }
    let matcher = if matcher_parts.is_empty() {
        String::new()
    } else {
        format!(", matcher: {}", matcher_parts.join(", "))
    };
    let capabilities = if hook.capabilities.is_empty() {
        String::new()
    } else {
        format!(
            ", capabilities=[{}]",
            hook.capabilities
                .iter()
                .map(|c| capability_label(*c))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let on_failure = match hook.on_failure {
        FailureBehavior::Warn => "warn",
        FailureBehavior::Block => "block",
    };
    format!(
        "  {} — {:?}, {action}, {state} ({}), blocking={}, on_failure={on_failure}, timeout={}s{matcher}{capabilities}",
        hook.name,
        hook.event,
        source.label(),
        hook.blocking,
        hook.timeout_secs,
    )
}

fn batching_label(batching: BatchingPolicy) -> &'static str {
    match batching {
        BatchingPolicy::Serialized => "serialized",
        BatchingPolicy::PathScoped => "path_scoped",
    }
}

pub(crate) fn format_config_diagnostics(diagnostics: &[ConfigDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return "Diagnostics: none.".to_string();
    }
    let mut lines = vec![format!("Diagnostics ({}):", diagnostics.len())];
    lines.extend(diagnostics.iter().map(|d| format!("  {d}")));
    lines.join("\n")
}

pub(crate) fn config_edit_message(config: &Config, scope: Option<ConfigEditScope>) -> String {
    let body = match scope {
        Some(ConfigEditScope::Project) => {
            format!("Project config: {}", config.layers.project_path.display())
        }
        Some(ConfigEditScope::Global) => {
            format!("Global config: {}", config.layers.global_path.display())
        }
        None => format!(
            "Project config: {}\nGlobal config: {}",
            config.layers.project_path.display(),
            config.layers.global_path.display(),
        ),
    };
    format!("{body}\n(edit it, then run /config validate; changes take effect on relaunch)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_view_validate_and_edit_variants() {
        assert_eq!(
            parse_config_command("/config"),
            Ok(ConfigCommandRequest::View)
        );
        assert_eq!(
            parse_config_command("/config validate"),
            Ok(ConfigCommandRequest::Validate)
        );
        assert_eq!(
            parse_config_command("/config edit"),
            Ok(ConfigCommandRequest::Edit(None))
        );
        assert_eq!(
            parse_config_command("/config edit project"),
            Ok(ConfigCommandRequest::Edit(Some(ConfigEditScope::Project)))
        );
        assert_eq!(
            parse_config_command("/config edit global"),
            Ok(ConfigCommandRequest::Edit(Some(ConfigEditScope::Global)))
        );
        assert!(parse_config_command("/config bogus").is_err());
        assert!(parse_config_command("/config edit bogus").is_err());
    }

    #[test]
    fn view_reports_empty_sections_and_no_diagnostics() {
        let config = Config::empty();
        let view = format_config_view(&config);
        assert!(view.contains("MCP servers: none configured."));
        assert!(view.contains("Hooks: none configured."));
        assert!(view.contains("Diagnostics: none."));
    }

    #[test]
    fn edit_message_names_the_requested_scope() {
        let config = Config::empty();
        assert!(
            config_edit_message(&config, Some(ConfigEditScope::Project))
                .starts_with("Project config:")
        );
        assert!(
            config_edit_message(&config, Some(ConfigEditScope::Global))
                .starts_with("Global config:")
        );
        let both = config_edit_message(&config, None);
        assert!(both.contains("Project config:") && both.contains("Global config:"));
    }
}
