use super::*;

/// The tools that mutate on-disk file content. Centralized so a new mutating
/// tool is added once, not drifted across the batcher's YOLO/path-scoping
/// rules and the run loop's Rust-edit/LSP-diagnostic hooks.
///
/// `apply_patch` touches many files in one call; it declares
/// `ParallelPolicy::Serialized` so the batcher routes it to `GlobalWrite`
/// before the single-`path` scoping here is ever reached. It appears in this
/// list so the run loop's self-review and Rust-diagnostic hooks recognize it
/// as a mutation (they extract its multiple target paths separately).
const MUTATION_TOOL_NAMES: [&str; 4] = ["write", "edit", "apply_patch", "rename_symbol"];

pub(super) fn is_mutation_tool(name: &str) -> bool {
    MUTATION_TOOL_NAMES.contains(&name)
}

enum ToolAccess {
    ReadPath(PathBuf),
    WritePath(PathBuf),
    GlobalWrite,
    /// Bash calls explicitly marked `parallel:true` or `run_in_background:true`.
    /// They may share a batch only with the same class. A shell command carries
    /// no trustworthy path scope, so it should not ride alongside reads, writes,
    /// or other serialized state changes until a narrower shell classifier can
    /// prove the command is read-only.
    ParallelBash,
    /// No shared state at all (`AlwaysSafe` tools) — never conflicts.
    Independent,
}

#[cfg(test)]
pub(super) fn tool_call_batches(
    tool_calls: &[ToolCall],
    registry: &ToolRegistry,
    project_root: &Path,
) -> Vec<Vec<ToolCall>> {
    tool_call_batches_with_yolo(
        tool_calls,
        registry,
        project_root,
        false,
        &std::collections::HashSet::new(),
    )
}

pub(super) fn tool_call_batches_with_yolo(
    tool_calls: &[ToolCall],
    registry: &ToolRegistry,
    project_root: &Path,
    yolo_enabled: bool,
    serialized_tool_names: &std::collections::HashSet<String>,
) -> Vec<Vec<ToolCall>> {
    let mut batches: Vec<Vec<(ToolCall, Vec<ToolAccess>)>> = Vec::new();
    for tool_call in tool_calls {
        let accesses = if serialized_tool_names.contains(&tool_call.name) {
            // A blocking `PreToolUse` hook matched this tool name
            // (`src/hooks/mod.rs::serialized_tool_names`): force it into the
            // same serialized bucket as an unparseable/unknown call,
            // regardless of the tool's own declared parallel policy, so a
            // hook that vetoes/rewrites a call never races another call to
            // the same tool in the same batch.
            vec![ToolAccess::GlobalWrite]
        } else {
            tool_call_accesses(
                &tool_call.name,
                &tool_call.arguments,
                registry,
                project_root,
                yolo_enabled,
            )
        };
        // A call must land after every batch it conflicts with, i.e. at
        // (highest conflicting batch index) + 1. Scan from the back and stop at
        // the first conflict — `n` (tool calls in one model turn) is small, so
        // this stays well within a handful of comparisons in practice.
        let mut target_batch = 0;
        for index in (0..batches.len()).rev() {
            if batches[index]
                .iter()
                .any(|(_, existing_accesses)| conflicts(existing_accesses, &accesses))
            {
                target_batch = index + 1;
                break;
            }
        }

        if target_batch == batches.len() {
            batches.push(vec![(tool_call.clone(), accesses)]);
        } else {
            batches[target_batch].push((tool_call.clone(), accesses));
        }
    }

    batches
        .into_iter()
        .map(|batch| batch.into_iter().map(|(tool_call, _)| tool_call).collect())
        .collect()
}

fn conflicts(left: &[ToolAccess], right: &[ToolAccess]) -> bool {
    left.iter()
        .any(|left| right.iter().any(|right| accesses_conflict(left, right)))
}

fn accesses_conflict(left: &ToolAccess, right: &ToolAccess) -> bool {
    match (left, right) {
        (ToolAccess::ParallelBash, ToolAccess::ParallelBash) => false,
        (ToolAccess::ParallelBash, _) | (_, ToolAccess::ParallelBash) => true,
        // `Independent` (AlwaysSafe tools) shares no state — never conflicts.
        (ToolAccess::Independent, _) | (_, ToolAccess::Independent) => false,
        // `GlobalWrite` (serialized side effects, unknown/unparseable calls)
        // conflicts with everything that can run.
        (ToolAccess::GlobalWrite, _) | (_, ToolAccess::GlobalWrite) => true,
        (ToolAccess::ReadPath(_), ToolAccess::ReadPath(_)) => false,
        (ToolAccess::ReadPath(left), ToolAccess::WritePath(right))
        | (ToolAccess::WritePath(left), ToolAccess::ReadPath(right))
        | (ToolAccess::WritePath(left), ToolAccess::WritePath(right)) => paths_overlap(left, right),
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right
        || left == Path::new(".")
        || right == Path::new(".")
        || left.starts_with(right)
        || right.starts_with(left)
}

fn tool_call_accesses(
    name: &str,
    arguments: &str,
    registry: &ToolRegistry,
    project_root: &Path,
    yolo_enabled: bool,
) -> Vec<ToolAccess> {
    let args: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(args) => args,
        // We can't tell what a call with unparseable arguments touches, so
        // serialize it rather than letting it run free.
        Err(_) => return vec![ToolAccess::GlobalWrite],
    };

    // Per-call overrides: bash{parallel: true} and
    // bash{run_in_background: true} opt the call into the parallel-bash class
    // so they can start alongside only other parallel bash calls. Those modes do
    // not update persistent Bash cwd.
    if name == "bash"
        && (args
            .get("parallel")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || args
                .get("run_in_background")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false))
    {
        return vec![ToolAccess::ParallelBash];
    }

    // PTY inspection and control share process-local lifecycle handles. Keep
    // every terminal action serialized until read-only snapshots and writes
    // have distinct, proven access classes.
    if name == "terminal" {
        return vec![ToolAccess::GlobalWrite];
    }

    if yolo_enabled && is_mutation_tool(name) {
        // YOLO permits absolute and parent-relative write paths. Serialize
        // write/edit calls so lexical aliases for the same external file
        // cannot race in one model turn.
        return vec![ToolAccess::GlobalWrite];
    }

    // A delegation (`agent`/`task`) classifies per target rather than as a
    // whole-tool `Serialized`: a read-only subagent shares no writable state, so
    // a fan-out of them runs in parallel (each modeled as a whole-tree read,
    // which still serializes against a real write in the same turn). A
    // write-capable target — or one that can't be resolved — stays serialized.
    if matches!(name, "agent" | "task") {
        let read_only = registry
            .get(name)
            .and_then(|tool| tool.delegation_is_read_only(&args))
            .unwrap_or(false);
        return if read_only {
            vec![ToolAccess::ReadPath(PathBuf::from("."))]
        } else {
            vec![ToolAccess::GlobalWrite]
        };
    }

    // Tool-declared policy decides how the call conflicts with others.
    // `Serialized` => GlobalWrite (conflicts with everything).
    // `AlwaysSafe` => Independent (no shared state).
    // `PathScoped` => per-tool path inference, falling back to
    // Independent when the args are unparseable or no path is present.
    match registry.get(name).map(|t| t.parallel_policy()) {
        Some(ParallelPolicy::Serialized) => vec![ToolAccess::GlobalWrite],
        Some(ParallelPolicy::AlwaysSafe) => vec![ToolAccess::Independent],
        // A registered path-scoped tool with no inferable path keeps the
        // author-declared parallel default.
        Some(ParallelPolicy::PathScoped) => {
            path_scoped_accesses(name, &args, project_root, ToolAccess::Independent)
        }
        // Not in the registry: the built-in read/write/etc. names still
        // path-scope (so unregistered-but-known calls behave), but a genuinely
        // unknown tool name serializes rather than running free.
        None => path_scoped_accesses(name, &args, project_root, ToolAccess::GlobalWrite),
    }
}

fn path_scoped_accesses(
    name: &str,
    args: &serde_json::Value,
    project_root: &Path,
    unknown_fallback: ToolAccess,
) -> Vec<ToolAccess> {
    match name {
        "read" | "read_region" | "read_symbol" => path_arg(args)
            .map(|path| {
                vec![ToolAccess::ReadPath(normalize_tool_path(
                    path,
                    project_root,
                ))]
            })
            .unwrap_or_else(|| vec![ToolAccess::Independent]),
        "glob" | "grep" | "symbol_search" | "definition" | "references" | "hover"
        | "workspace_symbol" => {
            vec![ToolAccess::ReadPath(normalize_tool_path(
                path_arg(args).unwrap_or("."),
                project_root,
            ))]
        }
        name if is_mutation_tool(name) => path_arg(args)
            .map(|path| {
                vec![ToolAccess::WritePath(normalize_tool_path(
                    path,
                    project_root,
                ))]
            })
            .unwrap_or_else(|| vec![ToolAccess::Independent]),
        // No path-based rule for this name. A registered path-scoped tool
        // keeps its author-declared parallelism; a genuinely unknown tool name
        // serializes — `unknown_fallback` carries that distinction so an
        // unrecognized call never races against other side effects in a turn.
        _ => vec![unknown_fallback],
    }
}

pub(super) fn path_arg(args: &serde_json::Value) -> Option<&str> {
    args.get("path").and_then(|value| value.as_str())
}

fn normalize_tool_path(path: &str, project_root: &Path) -> PathBuf {
    let input = Path::new(path);
    let normalized = normalize_path_components(input);
    if input.is_absolute()
        && let Some(relative) = strip_project_root(&normalized, project_root)
    {
        return relative;
    }

    normalized
}

fn normalize_path_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            other => normalized.push(other.as_os_str()),
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn strip_project_root(path: &Path, project_root: &Path) -> Option<PathBuf> {
    let normalized_root = normalize_path_components(project_root);
    path.strip_prefix(&normalized_root)
        .ok()
        .map(normalize_path_components)
        .or_else(|| {
            project_root.canonicalize().ok().and_then(|canonical_root| {
                path.strip_prefix(normalize_path_components(&canonical_root))
                    .ok()
                    .map(normalize_path_components)
            })
        })
}
