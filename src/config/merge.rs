//! Layer merging + precedence, for the two file layers ([`ConfigFile`]
//! instances already parsed from disk) plus the sandbox env stand-ins. CLI/env
//! precedence over the *project layer as a whole* (which file gets read) is
//! decided in [`super::load`]; this module only merges the two files' content.

use std::collections::BTreeMap;

use super::schema::ConfigFile;
use super::{
    ConfigSource, HookList, McpServerMap, SandboxConfig, UpdateConfig, VerificationConfig,
};

/// Merge the global and project layers into the sections a [`Config`]
/// exposes. `project_source` is `Project` normally, or `Env`/`Cli` when
/// `BONSAI_CONFIG`/`--config` redirected which file was read for that layer —
/// callers pass it through so provenance stays accurate.
pub(crate) fn merge(
    global: &ConfigFile,
    project: &ConfigFile,
    project_source: ConfigSource,
) -> (
    McpServerMap,
    HookList,
    SandboxConfig,
    VerificationConfig,
    UpdateConfig,
) {
    (
        merge_mcp_servers(global, project, project_source),
        merge_hooks(global, project, project_source),
        merge_sandbox(global, project),
        merge_verification(global, project),
        merge_update(global, project),
    )
}

/// Both `[update]` keys are genuine scalars: the higher-precedence layer's
/// explicit value wins. An invalid `pin` was already dropped (with a
/// diagnostic) at parse time, so the `.ok()` here never silently loses one.
fn merge_update(global: &ConfigFile, project: &ConfigFile) -> UpdateConfig {
    UpdateConfig {
        mode: project
            .update
            .mode
            .or(global.update.mode)
            .unwrap_or_default(),
        pin: project
            .update
            .pin
            .as_deref()
            .or(global.update.pin.as_deref())
            .and_then(|pin| semver::Version::parse(pin).ok()),
    }
}

fn merge_verification(global: &ConfigFile, project: &ConfigFile) -> VerificationConfig {
    VerificationConfig {
        test: project
            .verification
            .test
            .clone()
            .or_else(|| global.verification.test.clone()),
        build: project
            .verification
            .build
            .clone()
            .or_else(|| global.verification.build.clone()),
        after_edit: project
            .verification
            .after_edit
            .or(global.verification.after_edit)
            .unwrap_or_default(),
    }
}

/// Whole-entry replace per server name: a project `[mcp.servers.x]` replaces
/// the global one wholesale rather than patching individual fields.
fn merge_mcp_servers(
    global: &ConfigFile,
    project: &ConfigFile,
    project_source: ConfigSource,
) -> McpServerMap {
    let mut servers = BTreeMap::new();
    for (name, config) in &global.mcp.servers {
        servers.insert(name.clone(), (config.clone(), ConfigSource::Global));
    }
    for (name, config) in &project.mcp.servers {
        servers.insert(name.clone(), (config.clone(), project_source));
    }
    servers
}

/// Concatenated, global first then project — except a project hook with the
/// same `name` shadows (replaces in place) the global one instead of both
/// running, so `enabled = false` on a same-named project hook is the explicit
/// way to disable a global hook.
fn merge_hooks(
    global: &ConfigFile,
    project: &ConfigFile,
    project_source: ConfigSource,
) -> HookList {
    let mut hooks: HookList = global
        .hooks
        .iter()
        .cloned()
        .map(|hook| (hook, ConfigSource::Global))
        .collect();
    for hook in &project.hooks {
        match hooks
            .iter_mut()
            .find(|(existing, _)| existing.name == hook.name)
        {
            Some(slot) => *slot = (hook.clone(), project_source),
            None => hooks.push((hook.clone(), project_source)),
        }
    }
    hooks
}

/// `writable_roots` is additive (a union, like the env-var stand-in it
/// absorbs); `deny_network` is a genuine scalar, so the higher-precedence
/// layer's `Some` wins.
fn merge_sandbox(global: &ConfigFile, project: &ConfigFile) -> SandboxConfig {
    let mut writable_roots = global.sandbox.writable_roots.clone();
    for root in &project.sandbox.writable_roots {
        if !writable_roots.contains(root) {
            writable_roots.push(root.clone());
        }
    }
    SandboxConfig {
        writable_roots,
        deny_network: project.sandbox.deny_network.or(global.sandbox.deny_network),
    }
}
