//! Frontmatter parsing for on-disk resource definitions (skills, subagents).
//!
//! A resource file opens with a YAML frontmatter block delimited by `---` lines,
//! followed by a markdown body:
//!
//! ```text
//! ---
//! name: deploy
//! description: Build and ship the release
//! ---
//! <markdown instructions…>
//! ```
//!
//! One generic [`parse_frontmatter`] splits the block from the body and
//! deserializes the block into a typed target ([`SkillFrontmatter`] /
//! [`AgentFrontmatter`]). YAML (not TOML) is used because `.claude/skills` and
//! `.claude/agents` — which we read for cross-tool compatibility — use YAML.

use serde::Deserialize;

use crate::resource::activation::ActivationRule;

/// A parsed resource file: its typed frontmatter plus the trailing markdown body.
#[derive(Debug)]
pub(crate) struct Parsed<T> {
    pub frontmatter: T,
    pub body: String,
}

/// Why a resource file's frontmatter could not be parsed.
#[derive(thiserror::Error, Debug)]
pub(crate) enum FrontmatterError {
    #[error("missing leading --- frontmatter block")]
    Missing,
    #[error("unterminated frontmatter block")]
    Unterminated,
    #[error("invalid frontmatter: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
}

/// Split a leading `---`-delimited YAML frontmatter block from the markdown body
/// and deserialize the block into `T`.
///
/// Required fields absent from the block (e.g. `name`/`description` on the typed
/// targets) surface as [`FrontmatterError::Yaml`] via serde's "missing field"
/// error. Unknown extra keys are ignored so third-party `.claude` files carrying
/// fields we don't model still parse.
pub(crate) fn parse_frontmatter<T: serde::de::DeserializeOwned>(
    content: &str,
) -> Result<Parsed<T>, FrontmatterError> {
    // Tolerate a leading UTF-8 BOM some editors prepend.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or(FrontmatterError::Missing)?;

    let (yaml, body) = split_on_closing_delim(rest).ok_or(FrontmatterError::Unterminated)?;

    let frontmatter = serde_yaml_ng::from_str(yaml)?;
    Ok(Parsed {
        frontmatter,
        body: body.to_string(),
    })
}

/// Given the text *after* the opening `---` line, return `(yaml_block, body)`
/// split on the next line that is exactly `---` (ignoring a trailing `\r`).
/// Returns `None` when there is no closing delimiter.
fn split_on_closing_delim(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed == "---" {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((yaml, body));
        }
        offset += line.len();
    }
    None
}

/// Skill frontmatter. The body is the on-demand instruction set.
///
/// Capability-shaped fields are parsed so Bonsai can reject them explicitly
/// until the runtime enforces their meaning. Silently accepting them would
/// imply a tool or model boundary that does not exist.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    /// Tools a skill's instructions request access to.
    #[serde(rename = "allowed-tools", default)]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// When the skill should surface for a project (see
    /// [`crate::resource::activation`]). Absent means always active.
    #[serde(default)]
    pub activation: Option<ActivationRule>,
}

impl SkillFrontmatter {
    /// Reject recognized capability declarations that Bonsai cannot enforce.
    pub(crate) fn validate_supported_capabilities(&self) -> Result<(), FrontmatterValidationError> {
        if self.allowed_tools.is_some() {
            return Err(FrontmatterValidationError::unsupported(
                "skill",
                "allowed-tools",
                "skill-scoped tool access",
            ));
        }
        if self.model.is_some() {
            return Err(FrontmatterValidationError::unsupported(
                "skill",
                "model",
                "skill-specific model selection",
            ));
        }
        if self.effort.is_some() {
            return Err(FrontmatterValidationError::unsupported(
                "skill",
                "effort",
                "skill-specific reasoning effort",
            ));
        }
        Ok(())
    }
}

fn default_enabled() -> bool {
    true
}

/// Agent frontmatter. The body is the agent's system
/// prompt. `name`/`description`/`tools`/`model`/`effort`/`fallback_model`/
/// `fallback_effort`/`max_turns`/`enabled`/`view`/`surface`/`color` are consumed.
/// `permission` is recognized but rejected until agent-scoped permission
/// profiles are enforced.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct AgentFrontmatter {
    pub name: String,
    pub description: String,
    /// Scoped tool-name list the subagent's registry is filtered to.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Optional backup model selector used when the primary cannot be resolved
    /// or fails during a delegated run.
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// Optional reasoning-effort override for the backup model.
    #[serde(default)]
    pub fallback_effort: Option<String>,
    /// Optional model-turn budget when this definition runs as a subagent.
    #[serde(default, alias = "max-turns", alias = "maxTurns")]
    pub max_turns: Option<usize>,
    /// Declared permission/autonomy profile (can only narrow the parent's).
    #[serde(default)]
    pub permission: Option<String>,
    /// Whether the agent is active. A disabled agent is discovered but not
    /// advertised or runnable — useful for shipped/example definitions.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// UI surface when run as a switcher persona (`chat`/`todo`/`canvas`).
    #[serde(default)]
    pub view: Option<String>,
    /// How the agent can run (`mode` and/or `subagent`).
    #[serde(default)]
    pub surface: Option<Vec<String>>,
    /// Accent color for the persona label (a palette name or `#rrggbb`).
    #[serde(default)]
    pub color: Option<String>,
}

impl AgentFrontmatter {
    /// Reject recognized capability declarations that Bonsai cannot enforce.
    pub(crate) fn validate_supported_capabilities(&self) -> Result<(), FrontmatterValidationError> {
        if self.permission.is_some() {
            return Err(FrontmatterValidationError::unsupported(
                "agent",
                "permission",
                "agent-scoped permission profiles",
            ));
        }
        Ok(())
    }
}

/// A recognized resource declaration whose runtime semantics are unavailable.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error(
    "{resource} frontmatter field `{field}` is recognized but unsupported; remove it because Bonsai does not enforce {enforcement}"
)]
pub(crate) struct FrontmatterValidationError {
    resource: &'static str,
    field: &'static str,
    enforcement: &'static str,
}

impl FrontmatterValidationError {
    fn unsupported(resource: &'static str, field: &'static str, enforcement: &'static str) -> Self {
        Self {
            resource,
            field,
            enforcement,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_description_and_body() {
        let src = "---\nname: deploy\ndescription: Ship the release\n---\nDo the deploy.\n";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(src).unwrap();
        assert_eq!(parsed.frontmatter.name, "deploy");
        assert_eq!(parsed.frontmatter.description, "Ship the release");
        assert_eq!(parsed.body, "Do the deploy.\n");
    }

    #[test]
    fn parses_allowed_tools_flow_and_block_style() {
        let flow = "---\nname: a\ndescription: d\nallowed-tools: [bash, read]\n---\nbody";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(flow).unwrap();
        assert_eq!(
            parsed.frontmatter.allowed_tools,
            Some(vec!["bash".to_string(), "read".to_string()])
        );

        let block = "---\nname: a\ndescription: d\nallowed-tools:\n  - bash\n  - read\n---\nbody";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(block).unwrap();
        assert_eq!(
            parsed.frontmatter.allowed_tools,
            Some(vec!["bash".to_string(), "read".to_string()])
        );
    }

    #[test]
    fn unsupported_skill_capabilities_are_rejected_explicitly() {
        for (field, declaration) in [
            ("allowed-tools", "allowed-tools: [read]"),
            ("model", "model: codex:gpt-5.4"),
            ("effort", "effort: high"),
        ] {
            let src = format!("---\nname: a\ndescription: d\n{declaration}\n---\nbody");
            let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(&src).unwrap();
            let error = parsed
                .frontmatter
                .validate_supported_capabilities()
                .unwrap_err();
            let message = error.to_string();
            assert!(message.contains(field), "{message}");
            assert!(message.contains("recognized but unsupported"), "{message}");
        }
    }

    #[test]
    fn unsupported_agent_permission_is_rejected_explicitly() {
        let src = "---\nname: fixer\ndescription: d\npermission: allow-write\n---\nbody";
        let parsed: Parsed<AgentFrontmatter> = parse_frontmatter(src).unwrap();
        let error = parsed
            .frontmatter
            .validate_supported_capabilities()
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("`permission`"), "{message}");
        assert!(message.contains("recognized but unsupported"), "{message}");
    }

    #[test]
    fn parses_activation_block() {
        let src = "---\nname: a\ndescription: d\nactivation:\n  markers: [Cargo.toml]\n  extensions: [rs]\n---\nbody";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(src).unwrap();
        let activation = parsed.frontmatter.activation.expect("activation rule");
        assert_eq!(activation.markers, vec!["Cargo.toml".to_string()]);
        assert_eq!(activation.extensions, vec!["rs".to_string()]);
    }

    #[test]
    fn absent_activation_parses_as_none() {
        let src = "---\nname: a\ndescription: d\n---\nbody";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(src).unwrap();
        assert!(parsed.frontmatter.activation.is_none());
    }

    #[test]
    fn quoted_description_with_colon_round_trips() {
        let src = "---\nname: a\ndescription: \"Deploy: build & ship\"\n---\nbody";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(src).unwrap();
        assert_eq!(parsed.frontmatter.description, "Deploy: build & ship");
    }

    #[test]
    fn unknown_extra_keys_are_ignored() {
        let src = "---\nname: a\ndescription: d\nlicense: MIT\nversion: 3\n---\nbody";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(src).unwrap();
        assert_eq!(parsed.frontmatter.name, "a");
    }

    #[test]
    fn missing_required_field_is_rejected() {
        let src = "---\nname: a\n---\nbody";
        let err = parse_frontmatter::<SkillFrontmatter>(src).unwrap_err();
        assert!(matches!(err, FrontmatterError::Yaml(_)), "{err:?}");
    }

    #[test]
    fn missing_leading_delimiter_is_rejected() {
        let src = "name: a\ndescription: d\nbody";
        let err = parse_frontmatter::<SkillFrontmatter>(src).unwrap_err();
        assert!(matches!(err, FrontmatterError::Missing), "{err:?}");
    }

    #[test]
    fn unterminated_block_is_rejected() {
        let src = "---\nname: a\ndescription: d\nstill going";
        let err = parse_frontmatter::<SkillFrontmatter>(src).unwrap_err();
        assert!(matches!(err, FrontmatterError::Unterminated), "{err:?}");
    }

    #[test]
    fn agent_frontmatter_parses_tools_and_optional_fields() {
        let src = "---\nname: fixer\ndescription: Fix tests\ntools: [read, bash]\nmodel: codex:gpt-5.4\neffort: high\nfallback_model: anthropic:claude-sonnet-4-6\nfallback_effort: medium\n---\nYou fix failing tests.";
        let parsed: Parsed<AgentFrontmatter> = parse_frontmatter(src).unwrap();
        assert_eq!(parsed.frontmatter.name, "fixer");
        assert_eq!(
            parsed.frontmatter.tools,
            Some(vec!["read".to_string(), "bash".to_string()])
        );
        assert_eq!(parsed.frontmatter.model.as_deref(), Some("codex:gpt-5.4"));
        assert_eq!(parsed.frontmatter.effort.as_deref(), Some("high"));
        assert_eq!(
            parsed.frontmatter.fallback_model.as_deref(),
            Some("anthropic:claude-sonnet-4-6")
        );
        assert_eq!(
            parsed.frontmatter.fallback_effort.as_deref(),
            Some("medium")
        );
        assert_eq!(parsed.body, "You fix failing tests.");
    }

    #[test]
    fn agent_frontmatter_without_fallback_remains_compatible() {
        let src = "---\nname: fixer\ndescription: Fix tests\nmodel: codex:gpt-5.4\n---\nbody";
        let parsed: Parsed<AgentFrontmatter> = parse_frontmatter(src).unwrap();
        assert_eq!(parsed.frontmatter.fallback_model, None);
        assert_eq!(parsed.frontmatter.fallback_effort, None);
    }

    #[test]
    fn closing_delimiter_as_last_line_yields_empty_body() {
        let src = "---\nname: a\ndescription: d\n---";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(src).unwrap();
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn leading_bom_is_tolerated() {
        let src = "\u{feff}---\nname: a\ndescription: d\n---\nbody";
        let parsed: Parsed<SkillFrontmatter> = parse_frontmatter(src).unwrap();
        assert_eq!(parsed.frontmatter.name, "a");
        assert_eq!(parsed.body, "body");
    }
}
