//! The `skill` tool: progressive disclosure for skills. The Skills index
//! in the cacheable prompt prefix advertises what exists; when the model decides
//! a skill is relevant it calls `skill { name }` and the full `SKILL.md` body
//! enters the conversation tail as trusted context — so the always-on prefix
//! stays small and the prompt cache is preserved.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;

use crate::resource::skill::{SharedSkillRegistry, SkillRegistry, snapshot};
use crate::tool::schema::{closed_object, parse_args, string_property};
use crate::tool::{ParallelPolicy, Tool, ToolOutput};

#[derive(Debug)]
pub struct SkillTool {
    /// Shared, hot-swappable handle: a mid-session enable/disable is reflected
    /// here without a relaunch (see [`SharedSkillRegistry`]).
    skills: SharedSkillRegistry,
}

impl SkillTool {
    pub(crate) fn new(skills: SharedSkillRegistry) -> Self {
        Self { skills }
    }
}

#[derive(Debug, Deserialize)]
struct SkillArgs {
    name: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for a named skill listed in the Skills index. Returns the skill's trusted instructions to follow for the current task. Read the index first; call this only when a skill is relevant."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [(
                "name",
                string_property("The skill name exactly as shown in the Skills index."),
            )],
            &["name"],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::AlwaysSafe
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let SkillArgs { name } = parse_args("skill", args)?;
        // Snapshot the live registry so a just-disabled skill is no longer
        // loadable (and a just-enabled one is).
        let skills = snapshot(&self.skills);
        match skills.get(&name) {
            Some(def) => Ok(ToolOutput::TrustedContext {
                summary: format!("Loaded skill '{name}' into trusted context."),
                content: def.render_for_context(),
            }),
            None => Err(anyhow!(
                "Unknown skill '{name}'.{}",
                available_hint(&skills)
            )),
        }
    }
}

/// Guide the model back on track when it names a skill that doesn't exist.
fn available_hint(skills: &SkillRegistry) -> String {
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    if names.is_empty() {
        " No skills are defined in this project.".to_string()
    } else {
        format!(" Available skills: {}.", names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn registry(root: &Path) -> SharedSkillRegistry {
        let home = root.join("home");
        crate::resource::skill::shared_registry(SkillRegistry::load_from(root, &home))
    }

    fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
        let path = root.join(format!(".bonsai/skills/{name}/SKILL.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn loads_skill_body() {
        let dir = tempfile::TempDir::new().unwrap();
        write_skill(dir.path(), "deploy", "Ship it", "Run deploy.sh");
        let tool = SkillTool::new(registry(dir.path()));

        let result = tool
            .execute(serde_json::json!({ "name": "deploy" }))
            .await
            .unwrap();
        let ToolOutput::TrustedContext { summary, content } = result else {
            panic!("skill tool should return trusted context");
        };
        assert_eq!(summary, "Loaded skill 'deploy' into trusted context.");
        assert!(content.contains("Run deploy.sh"));
        assert!(content.contains("Skill: deploy"));
    }

    #[tokio::test]
    async fn unknown_skill_guides_with_available_names() {
        let dir = tempfile::TempDir::new().unwrap();
        write_skill(dir.path(), "deploy", "Ship it", "body");
        let tool = SkillTool::new(registry(dir.path()));

        let err = tool
            .execute(serde_json::json!({ "name": "nope" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown skill 'nope'"), "{err}");
        assert!(err.contains("deploy"), "{err}");
    }

    #[test]
    fn skill_tool_is_always_safe_and_closed_schema() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = SkillTool::new(registry(dir.path()));
        assert_eq!(tool.parallel_policy(), ParallelPolicy::AlwaysSafe);
        assert_eq!(tool.parameters_schema()["additionalProperties"], false);
    }
}
