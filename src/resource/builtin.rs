//! Skills compiled into the binary, mirroring the `models/builtin/`
//! pattern: each is a normal `SKILL.md` under `skills/builtin/<name>/` embedded
//! with `include_str!`, so it parses through the exact same frontmatter path as
//! on-disk skills and stays editable as plain markdown.
//!
//! Built-ins carry an `activation:` rule (see [`crate::resource::activation`])
//! scoping them to matching projects — `rust-writer` only surfaces where
//! `Cargo.toml`/`.rs` files exist, so unrelated projects pay nothing in the
//! prompt prefix. The deliberate exception is `ALWAYS_ACTIVE_BUILTINS`:
//! product skills about configuring bonsai itself, which no project marker
//! can predict. They have the lowest shadowing precedence: a project or
//! global skill of the same name replaces the built-in entirely.
//!
//! Adding a built-in = one new `skills/builtin/<name>/SKILL.md` + one entry in
//! [`BUILTIN_SKILL_SOURCES`]. The `all_builtins_parse_and_are_named_consistently`
//! test keeps the embedded set honest at CI time, and `resource::drift_tests`
//! holds the product skills' examples to the real config/catalog schemas.

use crate::resource::activation::ActivationRule;
use crate::resource::cap_body;
use crate::resource::discovery::Provenance;
use crate::resource::frontmatter::{Parsed, SkillFrontmatter, parse_frontmatter};
use crate::resource::skill::SkillDef;

/// An embedded skill source: its repo-relative path (used as the def's
/// `source_path` for display) and the raw `SKILL.md` content.
pub(crate) struct BuiltinSkillSource {
    pub path: &'static str,
    pub content: &'static str,
}

/// Product skills that deliberately omit an `activation:` rule and ride every
/// project's prompt prefix (one index line each). They cover configuring
/// bonsai itself — providers, themes, skills, hooks — which can be asked from
/// any project, so no file marker could gate them. Every other built-in must
/// scope itself; the invariant tests enforce both directions (test-only: no
/// runtime code branches on this, the rule lives in each skill's frontmatter).
#[cfg(test)]
pub(crate) const ALWAYS_ACTIVE_BUILTINS: &[&str] = &["customize", "provider-setup"];

/// Every skill shipped in the binary. Keep sorted by path.
pub(crate) const BUILTIN_SKILL_SOURCES: &[BuiltinSkillSource] = &[
    BuiltinSkillSource {
        path: "skills/builtin/customize/SKILL.md",
        content: include_str!("../../skills/builtin/customize/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/go-writer/SKILL.md",
        content: include_str!("../../skills/builtin/go-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/java-writer/SKILL.md",
        content: include_str!("../../skills/builtin/java-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/javascript-writer/SKILL.md",
        content: include_str!("../../skills/builtin/javascript-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/php-writer/SKILL.md",
        content: include_str!("../../skills/builtin/php-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/provider-setup/SKILL.md",
        content: include_str!("../../skills/builtin/provider-setup/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/python-writer/SKILL.md",
        content: include_str!("../../skills/builtin/python-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/ruby-writer/SKILL.md",
        content: include_str!("../../skills/builtin/ruby-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/rust-writer/SKILL.md",
        content: include_str!("../../skills/builtin/rust-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/shell-writer/SKILL.md",
        content: include_str!("../../skills/builtin/shell-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/typescript-writer/SKILL.md",
        content: include_str!("../../skills/builtin/typescript-writer/SKILL.md"),
    },
    BuiltinSkillSource {
        path: "skills/builtin/web-writer/SKILL.md",
        content: include_str!("../../skills/builtin/web-writer/SKILL.md"),
    },
];

/// Parse all embedded skills into defs plus their activation rules. A built-in
/// that fails to parse is a packaging bug: it is skipped with an error log
/// here and caught at CI time by the test below.
pub(crate) fn builtin_skill_defs() -> Vec<(SkillDef, Option<ActivationRule>)> {
    BUILTIN_SKILL_SOURCES
        .iter()
        .filter_map(|source| match parse_builtin(source) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                tracing::error!(path = source.path, error = %err, "malformed built-in skill");
                None
            }
        })
        .collect()
}

fn parse_builtin(
    source: &BuiltinSkillSource,
) -> anyhow::Result<(SkillDef, Option<ActivationRule>)> {
    let Parsed { frontmatter, body } = parse_frontmatter::<SkillFrontmatter>(source.content)?;
    frontmatter.validate_supported_capabilities()?;
    let (body, truncated) = cap_body(body);
    let def = SkillDef {
        name: frontmatter.name,
        description: frontmatter.description,
        body,
        source_path: source.path.into(),
        provenance: Provenance::Builtin,
        truncated,
    };
    Ok((def, frontmatter.activation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtins_parse_and_are_named_consistently() {
        let defs = builtin_skill_defs();
        assert_eq!(
            defs.len(),
            BUILTIN_SKILL_SOURCES.len(),
            "every embedded built-in must parse"
        );
        for (def, activation) in &defs {
            assert!(!def.description.is_empty());
            assert!(!def.body.trim().is_empty());
            assert!(
                !def.truncated,
                "built-in '{}' exceeds the body cap",
                def.name
            );
            // The frontmatter name must match the skills/builtin/<name>/ dir.
            let dir_name = def
                .source_path
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            assert_eq!(def.name, dir_name, "name/path mismatch for a built-in");
            if ALWAYS_ACTIVE_BUILTINS.contains(&def.name.as_str()) {
                // Product skills are the one sanctioned unconditioned kind —
                // and must stay unconditioned, so a stray rule can't quietly
                // hide "how do I configure bonsai" from most projects.
                assert!(
                    activation.is_none(),
                    "product skill '{}' must not declare activation",
                    def.name
                );
            } else {
                // Every other built-in must scope itself; an unconditioned
                // built-in would land in every project's prefix.
                let rule = activation.as_ref().expect("built-ins declare activation");
                assert!(!rule.markers.is_empty() || !rule.extensions.is_empty());
            }
        }
    }

    #[test]
    fn each_builtin_activates_on_its_ecosystem_and_not_before() {
        use crate::resource::activation::ActivationProbe;

        // One representative file per built-in that must flip it active.
        let samples: &[(&str, &str)] = &[
            ("go-writer", "go.mod"),
            ("java-writer", "pom.xml"),
            // Extension-gated (no marker), so the sample is a *.js file.
            ("javascript-writer", "index.js"),
            ("php-writer", "composer.json"),
            ("python-writer", "pyproject.toml"),
            ("ruby-writer", "Gemfile"),
            ("rust-writer", "Cargo.toml"),
            ("shell-writer", "install.sh"),
            ("typescript-writer", "tsconfig.json"),
            ("web-writer", "index.html"),
        ];
        let defs = builtin_skill_defs();
        assert_eq!(
            defs.len(),
            samples.len() + ALWAYS_ACTIVE_BUILTINS.len(),
            "a new built-in needs a sample row here (or an ALWAYS_ACTIVE_BUILTINS entry)"
        );

        for (name, sample) in samples {
            let (_, activation) = defs
                .iter()
                .find(|(def, _)| def.name == *name)
                .unwrap_or_else(|| panic!("missing built-in '{name}'"));
            let rule = activation.as_ref().expect("activation rule");

            let root = tempfile::TempDir::new().unwrap();
            assert!(
                !rule.is_active(&mut ActivationProbe::new(root.path())),
                "'{name}' must stay inactive in an empty project"
            );
            std::fs::write(root.path().join(sample), "x").unwrap();
            assert!(
                rule.is_active(&mut ActivationProbe::new(root.path())),
                "'{name}' must activate once {sample} exists"
            );
        }
    }
}
