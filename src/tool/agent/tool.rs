use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::catalog::{
    BUILTIN_AGENTS, SubagentRunLimits, builtin_agent, builtin_settings_model_chain,
    canonical_agent_tool, custom_subagent_limits, frontmatter_model_chain, merged_agent_entries,
    registry_grants_mutation,
};
use super::runner::{SubagentRunSpec, SubagentRunner};
use crate::agent::ExecutionLaneKind;
use crate::subagent::SubagentModelChain;
use crate::tool::schema::{boolean_property, closed_object, parse_args, string_property};
use crate::tool::{ParallelPolicy, Tool, ToolExecutionContext, ToolOutput, ToolRegistry};

pub struct AgentTool {
    name: &'static str,
    pub(super) runner: SubagentRunner,
    /// User-defined custom agents. Reserved built-in ids resolve first;
    /// every other name is read from this hot-swappable registry so the TUI can
    /// add or remove definitions mid-session.
    custom_agents: crate::resource::agent::SharedAgentRegistry,
    builtin_settings: crate::subagent::SharedBuiltinSubagentSettings,
    resolution: AgentResolution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentResolution {
    CustomAndBuiltin,
    BuiltinOnly,
}

impl std::fmt::Debug for AgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTool")
            .field("resolution", &self.resolution)
            .finish_non_exhaustive()
    }
}

impl AgentTool {
    #[cfg(test)]
    pub(crate) fn new(
        runner: SubagentRunner,
        custom_agents: crate::resource::agent::SharedAgentRegistry,
    ) -> Self {
        Self::new_with_settings(
            runner,
            custom_agents,
            crate::subagent::SharedBuiltinSubagentSettings::default(),
        )
    }

    pub(crate) fn new_with_settings(
        runner: SubagentRunner,
        custom_agents: crate::resource::agent::SharedAgentRegistry,
        builtin_settings: crate::subagent::SharedBuiltinSubagentSettings,
    ) -> Self {
        Self::new_named_with_settings("agent", runner, custom_agents, builtin_settings)
    }

    fn new_named_with_settings(
        name: &'static str,
        runner: SubagentRunner,
        custom_agents: crate::resource::agent::SharedAgentRegistry,
        builtin_settings: crate::subagent::SharedBuiltinSubagentSettings,
    ) -> Self {
        Self {
            name,
            runner,
            custom_agents,
            builtin_settings,
            resolution: AgentResolution::CustomAndBuiltin,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_builtin_only(
        name: &'static str,
        runner: SubagentRunner,
        custom_agents: crate::resource::agent::SharedAgentRegistry,
    ) -> Self {
        Self::new_builtin_only_with_settings(
            name,
            runner,
            custom_agents,
            crate::subagent::SharedBuiltinSubagentSettings::default(),
        )
    }

    pub(crate) fn new_builtin_only_with_settings(
        name: &'static str,
        runner: SubagentRunner,
        custom_agents: crate::resource::agent::SharedAgentRegistry,
        builtin_settings: crate::subagent::SharedBuiltinSubagentSettings,
    ) -> Self {
        Self {
            name,
            runner,
            custom_agents,
            builtin_settings,
            resolution: AgentResolution::BuiltinOnly,
        }
    }

    /// Resolve and run the requested subagent, carrying its token usage back even
    /// on error/timeout and surfacing any run error as the tool result (rather
    /// than an `Err` that would strand the subagent's spend).
    async fn run_delegation(
        &self,
        args: serde_json::Value,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput> {
        let AgentToolArgs {
            agent,
            prompt,
            run_in_background,
        } = parse_args(self.name, args)?;
        // Resolve and build the scoped registry before delegating, so a bad name
        // or tool list fails fast.
        let resolved = self.resolve(&agent)?;
        let grants_mutation = registry_grants_mutation(&resolved.registry);
        if let Some(path) = outside_project_path_in_prompt(&prompt, &self.runner.project_root) {
            anyhow::bail!(
                "Subagent task references existing path '{}' outside the current project root '{}'. Subagents are confined to the current project and cannot inspect it. Launch Bonsai from that project instead of retrying this delegation or falling back to shell commands in the current scope.",
                path.display(),
                self.runner.project_root.display(),
            );
        }
        if run_in_background && self.runner.has_background_wake() {
            if grants_mutation {
                anyhow::bail!(
                    "run_in_background is only supported for read-only subagents; run mutating subagents synchronously so permission prompts and edits stay in the active turn."
                );
            }
            let subtask_id = self.runner.run_in_background(
                &resolved.name,
                &resolved.instructions,
                &prompt,
                resolved.registry,
                resolved.model_chain,
                resolved.limits,
            );
            let message = format!(
                "Started background subagent {subtask_id} ({}). I will resume when it finishes.",
                resolved.name
            );
            return Ok(ToolOutput::SubagentStarted {
                subtask_id,
                message,
            });
        }
        let (result, usage, usage_turns, delegated_read_evidence) = self
            .runner
            .run_new(resolved.into_run_spec(prompt), cancellation)
            .await;
        let (text, status) = match result {
            Ok(text) => (text, crate::output::ToolExecutionStatus::Succeeded),
            Err(err) => (
                format!("Error: {err}"),
                crate::output::ToolExecutionStatus::Failed,
            ),
        };
        Ok(ToolOutput::TextWithUsage {
            text,
            status,
            usage,
            usage_turns,
            delegated_read_evidence,
            workspace_effect: if grants_mutation {
                crate::tool::ToolWorkspaceEffect::Unscoped
            } else {
                crate::tool::ToolWorkspaceEffect::NoMutation
            },
        })
    }

    /// Resolve an agent name to its framing + scoped tool registry. Built-in ids
    /// are reserved. A legacy same-name file may retain model and enabled
    /// settings, but cannot replace the compiled prompt, tools, or run limits.
    pub(super) fn resolve(&self, name: &str) -> Result<ResolvedAgent> {
        // Snapshot the live registry (the composer may swap it mid-session).
        let custom_agents = crate::resource::agent::snapshot(&self.custom_agents);
        if let Some(spec) = builtin_agent(name) {
            let settings = self.builtin_settings.get(spec.id);
            // Pre-DB releases stored built-in model/enable edits in a same-name
            // Markdown file. Every resolver mode may consume only those legacy
            // settings; prompt, tools, and limits always remain compiled.
            let legacy = if settings.is_none() {
                custom_agents.get(name)
            } else {
                None
            };
            let enabled = settings
                .as_ref()
                .map(|settings| settings.enabled)
                .or_else(|| legacy.map(|def| def.enabled))
                .unwrap_or(true);
            if !enabled {
                return Err(anyhow!("Agent '{name}' is disabled."));
            }
            return Ok(ResolvedAgent {
                name: spec.name.to_string(),
                instructions: spec.instructions.to_string(),
                registry: self.runner.read_only_registry().clone(),
                model_chain: settings
                    .as_ref()
                    .map(builtin_settings_model_chain)
                    .or_else(|| legacy.map(frontmatter_model_chain))
                    .unwrap_or_default(),
                limits: spec.budget.limits(),
            });
        }
        if self.resolution == AgentResolution::CustomAndBuiltin {
            match custom_agents.get(name) {
                Some(def) if def.enabled && def.allows_subagent() => {
                    return Ok(ResolvedAgent {
                        name: def.name.clone(),
                        instructions: def.instructions.clone(),
                        registry: self.scoped_registry(def.tools.as_deref())?,
                        model_chain: frontmatter_model_chain(def),
                        limits: custom_subagent_limits(def.max_turns),
                    });
                }
                Some(def) if !def.enabled => {
                    return Err(anyhow!("Agent '{name}' is disabled."));
                }
                Some(_) => {
                    return Err(anyhow!(
                        "Agent '{name}' is not available as a delegated subagent."
                    ));
                }
                None => {}
            }
        }
        let available: Vec<String> = match self.resolution {
            AgentResolution::CustomAndBuiltin => {
                merged_agent_entries(&custom_agents, &self.builtin_settings.snapshot())
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect()
            }
            AgentResolution::BuiltinOnly => BUILTIN_AGENTS
                .iter()
                .map(|spec| spec.name.to_string())
                .collect(),
        };
        Err(anyhow!(
            "Unknown agent '{name}'. Available: {}.",
            available.join(", ")
        ))
    }

    /// Build a scoped registry from a custom agent's declared `tools:`, filtering
    /// the full grantable registry. `None` keeps the read-only default set (so an
    /// unscoped delegation stays safe). A declared name that isn't grantable, or
    /// isn't in the subagent's full registry, is skipped (surfaced separately by
    /// `/agents`) rather than failing the whole delegation.
    fn scoped_registry(&self, tools: Option<&[String]>) -> Result<Arc<ToolRegistry>> {
        let Some(tools) = tools else {
            return Ok(self.runner.read_only_registry().clone());
        };
        let full_registry = self.runner.full_registry();
        let mut registry = ToolRegistry::new();
        for name in tools {
            if let Some(tool) =
                canonical_agent_tool(name).and_then(|canonical| full_registry.get(canonical))
            {
                registry.register(tool);
            }
        }
        Ok(Arc::new(registry))
    }
}
pub(super) fn outside_project_path_in_prompt(prompt: &str, project_root: &Path) -> Option<PathBuf> {
    let canonical_root = project_root.canonicalize().ok()?;
    prompt.split_whitespace().find_map(|token| {
        let token = token.trim_matches(|character: char| {
            matches!(
                character,
                '`' | '\''
                    | '"'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | ','
                    | ';'
                    | ':'
                    | '.'
                    | '!'
                    | '?'
            )
        });
        let path = Path::new(token);
        if !path.is_absolute() {
            return None;
        }
        let canonical = match path.canonicalize() {
            Ok(canonical) => canonical,
            Err(_) => {
                let without_location = trim_source_location(token);
                if without_location == token {
                    return None;
                }
                Path::new(without_location).canonicalize().ok()?
            }
        };
        (!canonical.starts_with(&canonical_root)).then_some(canonical)
    })
}

fn trim_source_location(path: &str) -> &str {
    let mut end = path.len();
    for _ in 0..2 {
        let Some(separator) = path[..end].rfind(':') else {
            break;
        };
        let suffix = &path[separator + 1..end];
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            break;
        }
        end = separator;
    }
    &path[..end]
}

/// A resolved agent ready to run: its framing prompt, scoped tool registry, and
/// the frontmatter model override (if any; a live `/subagents` override still wins
/// over this when the provider is minted).
pub(super) struct ResolvedAgent {
    pub(super) name: String,
    pub(super) instructions: String,
    pub(super) registry: Arc<ToolRegistry>,
    pub(super) model_chain: SubagentModelChain,
    pub(super) limits: SubagentRunLimits,
}

impl ResolvedAgent {
    fn into_run_spec(self, task: String) -> SubagentRunSpec {
        SubagentRunSpec {
            label: self.name,
            instructions: self.instructions,
            task,
            registry: self.registry,
            model_chain: self.model_chain,
            lane_kind: ExecutionLaneKind::Subagent,
            limits: self.limits,
        }
    }
}
#[derive(Debug, Deserialize)]
struct AgentToolArgs {
    agent: String,
    prompt: String,
    #[serde(default)]
    run_in_background: bool,
}

#[async_trait]
impl Tool for AgentTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::Delegated
    }

    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        match self.resolution {
            AgentResolution::CustomAndBuiltin => {
                "Delegate a scoped sub-task to a subagent and get back only its final \
conclusion, keeping your own context lean. The available subagents (built-in plus any the project \
defines) are listed in the Subagents index in the system prompt. Built-in subagents are read-only; \
custom agents can use only the tools granted by their frontmatter. To cover a big task from several \
angles at once — e.g. scanning the codebase for bugs — make multiple `agent` calls to read-only \
subagents in the SAME turn, each with a distinct, non-overlapping scope: they run in parallel and \
you resume once all of them finish. For broad read-only review/explore/research/security sweeps, prefer \
`run_in_background: true` and end your turn after launching them so your own prompt prefix stays \
cache-warm; background subagents report back when done. Do not use background mode for mutating \
custom agents. Subagents inherit the current project root; do not delegate work that requires an \
existing path outside it."
            }
            AgentResolution::BuiltinOnly => {
                "Delegate a scoped sub-task to a built-in read-only subagent and get back only its final \
conclusion, keeping your own context lean. Only the built-in explore, research, review, and security-review agents \
are available through this tool. Make several calls in one turn (each a distinct scope) to run them \
in parallel and resume when all finish. For broad review/explore/research/security sweeps, prefer \
`run_in_background: true` and end your turn after launching them so your own prompt prefix stays \
cache-warm; the built-in subagents report back when done. Built-in subagents inherit the current \
project root and cannot inspect existing paths outside it."
            }
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "agent",
                    string_property(
                        "The subagent to run, by name (see the Subagents index in the system prompt).",
                    ),
                ),
                (
                    "prompt",
                    string_property("The focused task or question for the subagent."),
                ),
                (
                    "run_in_background",
                    boolean_property(
                        "If true, start a read-only subagent in the background and return immediately.",
                    ),
                ),
            ],
            &["agent", "prompt"],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        // Conservative whole-tool default; the batcher prefers the per-call
        // `delegation_is_read_only` classification below, which lets a fan-out
        // of read-only subagents run in parallel.
        ParallelPolicy::Serialized
    }

    fn delegation_is_read_only(&self, args: &serde_json::Value) -> Option<bool> {
        let name = args.get("agent").and_then(|value| value.as_str())?;
        // An unresolvable target yields `None`, so the batcher serializes it.
        let resolved = self.resolve(name).ok()?;
        Some(!registry_grants_mutation(&resolved.registry))
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        // No execution context here, so no parent token is available — fall back
        // to a detached one. The run loop always calls `execute_with_context`,
        // which forwards the real token.
        self.run_delegation(args, CancellationToken::new()).await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: ToolExecutionContext,
    ) -> Result<ToolOutput> {
        // Forward the parent's cancellation token so a user cancel reaches the
        // nested run's own cooperative checks, not only via future-drop.
        let cancellation = context.cancellation_token().unwrap_or_default();
        self.run_delegation(args, cancellation).await
    }
}
