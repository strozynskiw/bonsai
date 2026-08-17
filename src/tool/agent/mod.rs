//! Scoped subagent delegation for the `agent` tool.
//!
//! Catalog metadata, run orchestration, lifecycle guards, and the model-facing
//! tool adapter live in separate modules so each responsibility can evolve
//! without expanding one monolithic adapter.

mod catalog;
mod lifecycle;
mod runner;
mod tool;

pub(crate) use catalog::{
    GRANTABLE_AGENT_TOOLS, agents_index_section_with_settings, builtin_agents,
    builtin_settings_model_chain, canonical_agent_tool, is_builtin_agent,
};
pub(crate) use runner::{
    SelfReviewRunOptions, SubagentProviderConfig, SubagentProviderFactory, SubagentRunner,
    SubagentToolRegistryFactory,
};
pub use tool::AgentTool;

#[cfg(test)]
use catalog::*;
#[cfg(test)]
use runner::*;
#[cfg(test)]
use tool::*;

#[cfg(test)]
mod tests;
