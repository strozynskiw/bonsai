use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::background::BackgroundTaskRegistry;
use crate::interaction::InteractionService;
use crate::permissions::PermissionManager;
use crate::sandbox::CommandSandbox;
use crate::terminal::TerminalRegistry;
use crate::tool::schema::{
    boolean_property, bounded_integer_property, closed_object, parse_args, string_property,
};
use crate::tool::{
    AuthorizationLedger, ParallelPolicy, ReadTracker, Tool, ToolExecutionContext, ToolOutput,
    diagnostic_excerpt_lines,
};
use crate::yolo::YoloMode;

pub(in crate::tool) mod command;
use command::{CommandAnalysis, analyze_command, extract_read_paths};

mod planning;
use planning::classify_planning_command;

mod output;
pub(crate) use output::BashOutputBudget;

mod session;
use session::{ConfinedFailures, EscapeApproval, SandboxEscapeGrants};

mod process;
use process::{CommandResult, CommandSummary, compact_summary_line};

mod policy;
use policy::{CommandApprovalRequest, emit_hook_warnings};

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    workdir: Option<String>,
    /// Opt in to running this call concurrently with other `parallel: true`
    /// bash calls in the same turn. The agent batcher will not serialize
    /// against other flagged calls, and the persistent shell `cwd` is *not*
    /// updated when this is set — the model is asserting that the command
    /// does not `cd`. Do not set on commands that change directory or rely
    /// on shell state from earlier calls.
    #[serde(default)]
    parallel: Option<bool>,
    /// Start the command as a process-local background task. Background
    /// commands snapshot cwd and never update the persistent shell cwd.
    #[serde(default)]
    run_in_background: Option<bool>,
    /// Start the command in a real process-local pseudo-terminal and return
    /// immediately. The terminal tool owns subsequent inspection and control.
    #[serde(default)]
    interactive: Option<bool>,
    /// Run this single command OUTSIDE the OS sandbox (requires user approval).
    #[serde(default)]
    escape_sandbox: Option<bool>,
}

const DEFAULT_TIMEOUT_SECS: u64 = 45;
const DEFAULT_BACKGROUND_TIMEOUT_SECS: u64 = 900;
const MAX_TIMEOUT_SECS: u64 = 900;
const FAILURE_SUMMARY_LINES: usize = 12;
const FAILURE_SUMMARY_LINE_CHARS: usize = 240;

/// Capability surface bound to a Bash tool instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BashCapability {
    /// The full coding-mode shell surface, guarded by the ordinary permission
    /// and autonomy policies.
    Coding,
    /// The fail-closed inspection and collaboration surface used while planning.
    Planning,
}

pub struct BashTool {
    // Fields are visible to the whole `bash` module tree so the cohesive
    // `impl BashTool` blocks can live in sibling files (session/process/policy)
    // without widening the surface beyond this subsystem.
    pub(in crate::tool::bash) project_root: PathBuf,
    pub(in crate::tool::bash) canonical_project_root: PathBuf,
    pub(in crate::tool::bash) permissions: PermissionManager,
    pub(in crate::tool::bash) read_tracker: ReadTracker,
    pub(in crate::tool::bash) interaction: Arc<InteractionService>,
    pub(in crate::tool::bash) background_tasks: Arc<BackgroundTaskRegistry>,
    pub(in crate::tool::bash) terminals: Arc<TerminalRegistry>,
    pub(in crate::tool::bash) shell: String,
    pub(in crate::tool::bash) cwd: Arc<Mutex<PathBuf>>,
    pub(in crate::tool::bash) yolo_mode: YoloMode,
    pub(in crate::tool::bash) sandbox: CommandSandbox,
    pub(in crate::tool::bash) output_budget: BashOutputBudget,
    /// Session-scoped sandbox-escape grants. In-memory only (escapes are never
    /// persisted) and shared by normal/SMOL bash tools for one session.
    pub(in crate::tool::bash) escape_grants: Arc<SandboxEscapeGrants>,
    /// Commands whose confined run failed with a sandbox-shaped denial; lifts
    /// the decline-unnecessary-escape shortcut so a retry can reach the prompt.
    pub(in crate::tool::bash) confined_failures: Arc<ConfinedFailures>,
    pub(in crate::tool::bash) hooks: Arc<crate::hooks::HookEngine>,
    pub(in crate::tool::bash) authorization_ledger: AuthorizationLedger,
    pub(in crate::tool::bash) capability: BashCapability,
    verification_cache: Arc<Mutex<HashMap<VerificationCacheKey, CachedVerificationOutput>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VerificationCacheKey {
    cwd: PathBuf,
    command: String,
    workspace_fingerprint: u64,
}

#[derive(Debug, Clone)]
struct CachedVerificationOutput {
    rendered: String,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
    truncation: Option<crate::tool::OutputTruncationContext>,
}

/// Runtime policy that must stay coherent for one Bash tool instance.
pub(crate) struct BashExecutionPolicy {
    yolo_mode: YoloMode,
    sandbox: CommandSandbox,
    hooks: Arc<crate::hooks::HookEngine>,
    authorization_ledger: AuthorizationLedger,
    verification_cache: Arc<Mutex<HashMap<VerificationCacheKey, CachedVerificationOutput>>>,
}

impl BashExecutionPolicy {
    pub(crate) fn new(
        yolo_mode: YoloMode,
        sandbox: CommandSandbox,
        hooks: Arc<crate::hooks::HookEngine>,
        authorization_ledger: AuthorizationLedger,
    ) -> Self {
        Self {
            yolo_mode,
            sandbox,
            hooks,
            authorization_ledger,
            verification_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl fmt::Debug for BashExecutionPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BashExecutionPolicy")
            .field("yolo_mode", &self.yolo_mode)
            .field("sandbox", &self.sandbox)
            .field("hooks", &"configured")
            .field("authorization_ledger", &"configured")
            .finish()
    }
}

/// Cohesive construction context for a Bash tool bound to one runtime.
pub(crate) struct BashRuntimeDeps {
    project_root: PathBuf,
    permissions: PermissionManager,
    read_tracker: ReadTracker,
    interaction: Arc<InteractionService>,
    background_tasks: Arc<BackgroundTaskRegistry>,
    terminals: Arc<TerminalRegistry>,
    execution_policy: BashExecutionPolicy,
}

impl BashRuntimeDeps {
    pub(crate) fn new(
        project_root: PathBuf,
        permissions: PermissionManager,
        read_tracker: ReadTracker,
        interaction: Arc<InteractionService>,
        background_tasks: Arc<BackgroundTaskRegistry>,
        terminals: Arc<TerminalRegistry>,
        execution_policy: BashExecutionPolicy,
    ) -> Self {
        Self {
            project_root,
            permissions,
            read_tracker,
            interaction,
            background_tasks,
            terminals,
            execution_policy,
        }
    }
}

impl fmt::Debug for BashRuntimeDeps {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BashRuntimeDeps")
            .field("project_root", &self.project_root)
            .field("permissions", &"configured")
            .field("read_tracker", &"configured")
            .field("interaction", &"configured")
            .field("background_tasks", &"configured")
            .field("terminals", &"configured")
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

impl BashTool {
    #[cfg(test)]
    pub fn new(
        project_root: PathBuf,
        permissions: PermissionManager,
        read_tracker: ReadTracker,
        interaction: Arc<InteractionService>,
    ) -> Self {
        Self::with_background_tasks_and_yolo_mode(
            project_root,
            permissions,
            read_tracker,
            interaction,
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::new(),
            CommandSandbox::disabled(),
        )
    }

    #[cfg(test)]
    pub fn with_background_tasks(
        project_root: PathBuf,
        permissions: PermissionManager,
        read_tracker: ReadTracker,
        interaction: Arc<InteractionService>,
        background_tasks: Arc<BackgroundTaskRegistry>,
    ) -> Self {
        Self::with_background_tasks_and_yolo_mode(
            project_root,
            permissions,
            read_tracker,
            interaction,
            background_tasks,
            YoloMode::new(),
            CommandSandbox::disabled(),
        )
    }

    #[cfg(test)]
    pub fn with_background_tasks_and_yolo_mode(
        project_root: PathBuf,
        permissions: PermissionManager,
        read_tracker: ReadTracker,
        interaction: Arc<InteractionService>,
        background_tasks: Arc<BackgroundTaskRegistry>,
        yolo_mode: YoloMode,
        sandbox: CommandSandbox,
    ) -> Self {
        let terminals = Arc::new(TerminalRegistry::with_sandbox(sandbox.clone()));
        let execution_policy = BashExecutionPolicy::new(
            yolo_mode,
            sandbox,
            Arc::new(crate::hooks::HookEngine::disabled()),
            AuthorizationLedger::disabled(),
        );
        Self::from_runtime(BashRuntimeDeps::new(
            project_root,
            permissions,
            read_tracker,
            interaction,
            background_tasks,
            terminals,
            execution_policy,
        ))
    }

    pub(crate) fn from_runtime(deps: BashRuntimeDeps) -> Self {
        let BashRuntimeDeps {
            project_root,
            permissions,
            read_tracker,
            interaction,
            background_tasks,
            terminals,
            execution_policy,
        } = deps;
        let BashExecutionPolicy {
            yolo_mode,
            sandbox,
            hooks,
            authorization_ledger,
            verification_cache,
        } = execution_policy;
        let canonical_project_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.clone());
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self {
            project_root,
            canonical_project_root: canonical_project_root.clone(),
            permissions,
            read_tracker,
            interaction,
            background_tasks,
            terminals,
            shell,
            cwd: Arc::new(Mutex::new(canonical_project_root)),
            yolo_mode,
            sandbox,
            output_budget: BashOutputBudget::normal(),
            escape_grants: Arc::new(SandboxEscapeGrants::default()),
            confined_failures: Arc::new(ConfinedFailures::default()),
            hooks,
            authorization_ledger,
            capability: BashCapability::Coding,
            verification_cache,
        }
    }

    pub(crate) fn with_shared_session_and_output_budget(
        &self,
        output_budget: BashOutputBudget,
    ) -> Self {
        Self {
            project_root: self.project_root.clone(),
            canonical_project_root: self.canonical_project_root.clone(),
            permissions: self.permissions.clone(),
            read_tracker: self.read_tracker.clone(),
            interaction: self.interaction.clone(),
            background_tasks: self.background_tasks.clone(),
            terminals: self.terminals.clone(),
            shell: self.shell.clone(),
            cwd: self.cwd.clone(),
            yolo_mode: self.yolo_mode.clone(),
            sandbox: self.sandbox.clone(),
            output_budget,
            escape_grants: self.escape_grants.clone(),
            confined_failures: self.confined_failures.clone(),
            hooks: self.hooks.clone(),
            authorization_ledger: self.authorization_ledger.clone(),
            capability: self.capability,
            verification_cache: self.verification_cache.clone(),
        }
    }

    /// Bind this session's shared shell state to the restricted planning-mode
    /// capability. This is intentionally a separate instance so coding Bash
    /// keeps its existing surface and policy.
    pub(crate) fn for_planning(&self) -> Self {
        let mut tool = self.with_shared_session_and_output_budget(self.output_budget);
        tool.capability = BashCapability::Planning;
        tool
    }
}

fn canonical_check_command(command: &str) -> Option<String> {
    let mut base = command.trim();
    if let Some((left, right)) = base.rsplit_once('|') {
        let tail = right.split_whitespace().collect::<Vec<_>>();
        let is_head = matches!(tail.as_slice(), ["head"])
            || matches!(tail.as_slice(), ["head", _])
            || matches!(tail.as_slice(), ["head", "-n", _]);
        if !is_head {
            return None;
        }
        base = left.trim_end();
    }
    base = base.strip_suffix("2>&1").map(str::trim_end).unwrap_or(base);
    if base.contains("&&") || base.contains(';') || base.contains("||") {
        return None;
    }
    let mut tokens = base.split_whitespace().peekable();
    if tokens.next() != Some("cargo") || !matches!(tokens.next(), Some("check" | "clippy")) {
        return None;
    }
    let mut normalized = vec!["cargo".to_string()];
    normalized.push(
        base.split_whitespace()
            .nth(1)
            .unwrap_or("check")
            .to_string(),
    );
    while let Some(token) = tokens.next() {
        if token == "--quiet" || token.starts_with("--message-format=") {
            continue;
        }
        if token == "--message-format" {
            let _ = tokens.next();
            continue;
        }
        normalized.push(token.to_string());
    }
    Some(normalized.join(" "))
}

fn structured_cargo_command(canonical: &str) -> String {
    if let Some((cargo_args, rustc_args)) = canonical.split_once(" -- ") {
        format!("{cargo_args} --message-format=json --quiet -- {rustc_args}")
    } else {
        format!("{canonical} --message-format=json --quiet")
    }
}

fn workspace_fingerprint(root: &Path) -> u64 {
    let mut paths = ["Cargo.toml", "Cargo.lock", "build.rs"]
        .into_iter()
        .map(|path| root.join(path))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    let src = root.join("src");
    if src.is_dir() {
        paths.extend(
            walkdir::WalkDir::new(&src)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| entry.into_path()),
        );
    }
    paths.sort_unstable();
    let mut hasher = DefaultHasher::new();
    for path in paths {
        path.strip_prefix(root).unwrap_or(&path).hash(&mut hasher);
        if let Ok(metadata) = std::fs::metadata(&path) {
            metadata.len().hash(&mut hasher);
            metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
                .hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[async_trait]
impl Tool for BashTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::SelfAuthorized
    }

    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        match self.capability {
            BashCapability::Coding => {
                "Run a shell command from the persistent project cwd. Use workdir instead of a repo `cd`; \
                 supports bounded foreground, background task, PTY, parallel, and approved sandbox-escape modes."
            }
            BashCapability::Planning => {
                "Run a restricted planning command: safe local inspection or approved gh/glab issue and pull/merge-request collaboration. Foreground only; shell syntax, redirects, project mutation, and sandbox escape are unavailable."
            }
        }
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut properties = vec![
            (
                "command",
                string_property(
                    "Command to execute in the persistent cwd (initially the project root).",
                ),
            ),
            (
                "timeout",
                bounded_integer_property(
                    "Timeout in seconds (foreground default: 45; background default: 900; max: 900)",
                    Some(1),
                    Some(MAX_TIMEOUT_SECS as i64),
                ),
            ),
            (
                "workdir",
                string_property("Optional project-relative working directory"),
            ),
            (
                "parallel",
                boolean_property(
                    "Run with sibling parallel:true calls; does not update persistent cwd",
                ),
            ),
            (
                "run_in_background",
                boolean_property("Return a background task ID immediately (default timeout: 900s)"),
            ),
            (
                "interactive",
                boolean_property(
                    "Return a PTY ID; incompatible with parallel, background, or escape",
                ),
            ),
            (
                "escape_sandbox",
                boolean_property(
                    "Request approval to run outside the sandbox; foreground non-PTY only",
                ),
            ),
        ];
        if self.capability == BashCapability::Planning {
            properties.truncate(3);
        }
        closed_object(properties, &["command"])
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        self.execute_inner(args, None).await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: ToolExecutionContext,
    ) -> Result<ToolOutput> {
        self.execute_inner(args, Some(context)).await
    }
}

impl BashTool {
    async fn execute_inner(
        &self,
        args: serde_json::Value,
        context: Option<ToolExecutionContext>,
    ) -> Result<ToolOutput> {
        let args: BashArgs = parse_args("bash tool", args)?;

        if args.command.is_empty() {
            anyhow::bail!("command is required");
        }

        let run_in_background = args.run_in_background.unwrap_or(false);
        let interactive = args.interactive.unwrap_or(false);
        let parallel = args.parallel.unwrap_or(false);
        let escape_sandbox = args.escape_sandbox.unwrap_or(false);
        if interactive && (run_in_background || parallel || escape_sandbox) {
            anyhow::bail!(
                "interactive mode cannot be combined with parallel, run_in_background, or escape_sandbox"
            );
        }
        if escape_sandbox && run_in_background {
            anyhow::bail!(
                "escape_sandbox is not supported for background tasks; run it in the foreground."
            );
        }
        let timeout_secs = effective_timeout_secs(args.timeout, run_in_background || interactive);
        let cwd = self.resolve_workdir(args.workdir.as_deref()).await?;
        let requested_command = self
            .normalize_redundant_leading_cd(&args.command, &cwd)
            .await;
        let planning_command = (self.capability == BashCapability::Planning)
            .then(|| classify_planning_command(&requested_command))
            .transpose()?;
        if planning_command.is_some()
            && (run_in_background || interactive || parallel || escape_sandbox)
        {
            anyhow::bail!(
                "planning Bash only supports confined foreground commands; background, interactive, parallel, and sandbox escape are unavailable"
            );
        }
        let canonical_check = planning_command
            .is_none()
            .then(|| canonical_check_command(&requested_command))
            .flatten();
        let command = planning_command
            .as_ref()
            .map(|command| command.command().to_string())
            .or_else(|| canonical_check.as_deref().map(structured_cargo_command))
            .unwrap_or_else(|| requested_command.clone());
        let analysis = analyze_command(&command);

        // Resolve whether this call genuinely needs to leave confinement before
        // command authorization. When both gates would prompt, the command gate
        // absorbs the escape into one sandbox-warning decision.
        let escape_requested = escape_sandbox && self.sandbox.is_active();
        let escape_auto_declined = escape_requested
            && escape_is_unnecessary(&analysis)
            && !self.confined_failures.contains(&cwd, &command);
        if escape_auto_declined {
            tracing::debug!(
                command = %command,
                "declining unnecessary sandbox escape: workspace-only command"
            );
        }
        let escape_needed = escape_requested && !escape_auto_declined;
        let command_approval_request =
            if escape_needed && self.escape_requires_prompt(&cwd, &command, &analysis) {
                CommandApprovalRequest::CommandAndSandbox { cwd: &cwd }
            } else {
                CommandApprovalRequest::CommandOnly
            };

        let origin = context.as_ref().and_then(|ctx| ctx.origin());
        let combined_escape = if let Some(planning_command) = &planning_command {
            self.authorize_planning_command(&command, &analysis, planning_command, origin)
                .await?;
            EscapeApproval::None
        } else {
            self.authorize_command(&command, &analysis, origin, command_approval_request)
                .await?
        };

        let pre_outcome = self
            .hooks
            .fire(
                crate::hooks::HookEvent::PreBash,
                crate::hooks::HookContext {
                    tool_name: Some("bash"),
                    command: Some(&command),
                    ..Default::default()
                },
            )
            .await;
        emit_hook_warnings(context.as_ref(), &pre_outcome.warnings);
        if let crate::hooks::HookDecision::Block { reason } = pre_outcome.decision {
            anyhow::bail!(reason);
        }

        let verification_cache_key =
            canonical_check
                .as_ref()
                .map(|canonical| VerificationCacheKey {
                    cwd: cwd.clone(),
                    command: canonical.clone(),
                    workspace_fingerprint: workspace_fingerprint(&self.canonical_project_root),
                });
        if !escape_sandbox
            && !run_in_background
            && !interactive
            && let Some(key) = verification_cache_key.as_ref()
            && let Some(cached) = self.verification_cache.lock().await.get(key).cloned()
        {
            return Ok(ToolOutput::Command {
                rendered: format!(
                    "[verification cache hit: workspace fingerprint unchanged]\n\n{}",
                    cached.rendered
                ),
                stdout: cached.stdout,
                stderr: cached.stderr,
                exit_code: cached.exit_code,
                timed_out: cached.timed_out,
                truncation: cached.truncation,
            });
        }

        // A command that needed both approvals already received them in the
        // combined modal. Otherwise resolve cached, automatic safe-retry, or
        // standalone sandbox authorization now.
        let escape = if combined_escape.escaped() {
            combined_escape
        } else if escape_needed {
            self.authorize_escape(&cwd, &command, &analysis, origin)
                .await?
        } else {
            EscapeApproval::None
        };

        if interactive {
            let terminal = self
                .terminals
                .start(
                    &self.shell,
                    &command,
                    &cwd,
                    timeout_secs,
                    context
                        .as_ref()
                        .map(|context| context.tool_call_id().to_string()),
                )
                .await?;
            let message = format!(
                "Started interactive terminal {} (timeout {}s) in {}. Use the terminal tool to read, send bounded non-secret input, resize, interrupt, or stop it.",
                terminal.id,
                timeout_secs,
                cwd.display()
            );
            return Ok(ToolOutput::BackgroundTaskStarted {
                task_id: terminal.id,
                message,
            });
        }

        if run_in_background {
            let task = self
                .background_tasks
                .start(&self.shell, &command, &cwd, timeout_secs)
                .await?;
            let message = format!(
                "Started background task {} (timeout {}s) in {}",
                task.id,
                timeout_secs,
                cwd.display()
            );
            return Ok(ToolOutput::BackgroundTaskStarted {
                task_id: task.id,
                message,
            });
        }

        let CommandResult {
            mut stdout,
            mut stderr,
            mut body,
            truncation,
            exit_code,
            timed_out,
            summary,
            confined,
        } = self
            .run_command(
                &command,
                &cwd,
                timeout_secs,
                escape.escaped(),
                planning_command
                    .as_ref()
                    .is_some_and(|command| command.permits_network()),
                context.clone(),
            )
            .await?;

        if canonical_check.is_some() {
            body = crate::tool::diagnostics::format_cargo_json_for_bash(&stdout, &stderr);
            stdout = body.clone();
            stderr.clear();
        }

        let post_excerpt = crate::hooks::truncate_excerpt(&body);
        let post_outcome = self
            .hooks
            .fire(
                crate::hooks::HookEvent::PostBash,
                crate::hooks::HookContext {
                    tool_name: Some("bash"),
                    command: Some(&command),
                    exit_code,
                    output_excerpt: Some(&post_excerpt),
                    ..Default::default()
                },
            )
            .await;
        emit_hook_warnings(context.as_ref(), &post_outcome.warnings);

        // Remember sandbox-shaped confined failures so a follow-up
        // escape_sandbox=true retry for this exact command reaches the real
        // approval prompt instead of being auto-declined again.
        if confined
            && !timed_out
            && matches!(exit_code, Some(code) if code != 0)
            && failure_looks_sandbox_related(&body)
        {
            self.confined_failures.record(&cwd, &command);
        }

        let mut rendered = Self::finalize_foreground(
            &requested_command,
            &body,
            exit_code,
            timed_out,
            confined,
            escape,
            escape_auto_declined,
            &summary,
        );

        self.mark_read_files(&cwd, &analysis, &stdout).await;
        // Parallel-mode calls do not update the persistent shell `cwd` —
        // the model is asserting no `cd` in exchange for concurrent
        // execution. Updating here would race with other in-flight calls.
        if !args.parallel.unwrap_or(false) {
            self.update_cwd(&cwd, analysis.permission_command()).await;
        }
        // Make persistent cwd drift visible while it lasts: `cd` persists
        // across bash calls, and models that forget this misread the next
        // failure as path/sandbox trouble (observed live: several recovery
        // rounds re-deriving "the cwd changed"). One line, only while the
        // tracked cwd is away from the project root.
        let persistent_cwd = self.cwd.lock().await.clone();
        if persistent_cwd != self.canonical_project_root {
            let shown = persistent_cwd
                .strip_prefix(&self.canonical_project_root)
                .map(|rel| format!("./{}", rel.display()))
                .unwrap_or_else(|_| persistent_cwd.display().to_string());
            Self::push_paragraph(
                &mut rendered,
                &format!(
                    "[shell cwd is {shown} and persists across bash calls; pass workdir or `cd` to change it]"
                ),
            );
        }

        if let Some(key) = verification_cache_key {
            // Cargo may legitimately rewrite Cargo.lock while checking. Cache
            // the state that produced the result, not the pre-execution state,
            // so the immediately following equivalent check can reuse it.
            let key = VerificationCacheKey {
                workspace_fingerprint: workspace_fingerprint(&self.canonical_project_root),
                ..key
            };
            self.verification_cache.lock().await.insert(
                key,
                CachedVerificationOutput {
                    rendered: rendered.clone(),
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                    exit_code,
                    timed_out,
                    truncation: truncation.clone(),
                },
            );
        }

        if planning_command
            .as_ref()
            .is_some_and(|command| command.permits_network())
        {
            let remote_output = if stderr.is_empty() {
                stdout.as_str()
            } else if stdout.is_empty() {
                stderr.as_str()
            } else {
                &format!("stdout:\n{stdout}\n\nstderr:\n{stderr}")
            };
            return Ok(ToolOutput::untrusted_context_with_status(
                "GitHub/GitLab CLI output",
                remote_output,
                if timed_out || !matches!(exit_code, Some(0)) {
                    crate::tool::ToolExecutionStatus::Failed
                } else {
                    crate::tool::ToolExecutionStatus::Succeeded
                },
            ));
        }

        Ok(ToolOutput::Command {
            rendered,
            stdout,
            stderr,
            exit_code,
            timed_out,
            truncation,
        })
    }

    /// Assemble the model-visible rendering of a finished foreground command:
    /// exit-code frame, failure summary, sandbox-escape audit/nudge, and the
    /// command-summary footer. Pure string assembly — unit-testable without
    /// spawning a process.
    #[allow(clippy::too_many_arguments)]
    fn finalize_foreground(
        command: &str,
        body: &str,
        exit_code: Option<i32>,
        timed_out: bool,
        confined: bool,
        escape: EscapeApproval,
        escape_auto_declined: bool,
        summary: &CommandSummary,
    ) -> String {
        let mut rendered = Self::frame_output(body, exit_code, timed_out);
        if let Some(failure) = Self::failure_summary(command, body, exit_code, timed_out) {
            rendered = if rendered.is_empty() {
                failure
            } else {
                format!("{failure}\n\n{rendered}")
            };
        }
        if let Some(hint) = missing_command_hint(body, exit_code) {
            Self::push_paragraph(&mut rendered, hint);
        }
        match escape {
            EscapeApproval::None => {
                // Soft nudge: a confined command failed *because the sandbox
                // blocked it*. Gated on a denial signature in the output, not on
                // failure alone — otherwise every ordinary non-zero exit (a
                // failing test, a clippy warning under a `git commit` pre-commit
                // hook, a bad path) told the model to escape, and it would burn a
                // user escape-approval on a command the sandbox never touched
                // (writes to `.git/` and the workspace succeed confined). See
                // `failure_looks_sandbox_related`.
                if confined
                    && !timed_out
                    && matches!(exit_code, Some(code) if code != 0)
                    && failure_looks_sandbox_related(body)
                {
                    // Two variants so the guidance is never a dead end: when the
                    // escape request was auto-declined (workspace-only git), say
                    // so and promise the retry reaches the prompt — the failure
                    // was recorded, which lifts the decline shortcut.
                    let nudge = if escape_auto_declined {
                        "This failed while confined to the sandbox — the error looks like a blocked write or network access. Your escape_sandbox request was declined because the command looked workspace-only, but this failure lifts that shortcut: retry with escape_sandbox=true and it will reach the approval prompt."
                    } else {
                        "This failed while confined to the sandbox — the error looks like a blocked write or network access. If it legitimately must write outside the project root or use the network, retry with escape_sandbox=true."
                    };
                    Self::push_paragraph(&mut rendered, nudge);
                }
            }
            scope => {
                // Structured audit on every escaped run (incl. cached). The visible
                // transcript banner is emitted by the TUI escalation handler.
                tracing::warn!(
                    command = %command,
                    grant = scope.label(),
                    "bash command ran outside the sandbox (approved escape)"
                );
            }
        }
        Self::push_paragraph(&mut rendered, &summary.footer(command));
        rendered
    }
}

fn missing_command_hint(body: &str, exit_code: Option<i32>) -> Option<&'static str> {
    let body = body.to_ascii_lowercase();
    if exit_code != Some(127)
        && !body.contains("command not found")
        && !body.contains("not recognized as")
    {
        return None;
    }
    if body.contains("rg") || body.contains("ripgrep") {
        return Some("`rg` is unavailable; retry the search with `grep -R`.");
    }
    if body.contains("fd") {
        return Some("`fd` is unavailable; retry the path search with `find`.");
    }
    if body.contains("sqlite3") {
        return Some(
            "`sqlite3` is unavailable. The project-context data-surfaces note lists the Bonsai database path and key tables; use an installed SQLite-capable project tool instead of guessing columns.",
        );
    }
    if body.contains("jq") {
        return Some(
            "`jq` is unavailable; narrow the producer output or use a project-native parser.",
        );
    }
    None
}

impl BashTool {
    /// Frame the combined command body. The command summary footer is appended
    /// separately so it remains the final parseable section.
    fn frame_output(body: &str, exit_code: Option<i32>, timed_out: bool) -> String {
        let mut result = body.to_string();

        if result.is_empty() && !timed_out && matches!(exit_code, Some(0)) {
            result.push_str("Command completed successfully (no output)");
        }

        result
    }

    fn failure_summary(
        command: &str,
        body: &str,
        exit_code: Option<i32>,
        timed_out: bool,
    ) -> Option<String> {
        if !timed_out && !matches!(exit_code, Some(code) if code != 0) {
            return None;
        }

        let mut lines = vec![
            "[Command failure summary]".to_string(),
            format!("command: {}", compact_summary_line(command)),
            format!(
                "exit_code: {}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".to_string())
            ),
            format!("timed_out: {timed_out}"),
        ];
        let excerpt =
            diagnostic_excerpt_lines(body, FAILURE_SUMMARY_LINES, FAILURE_SUMMARY_LINE_CHARS);
        if !excerpt.is_empty() {
            lines.push("diagnostic_excerpt:".to_string());
            lines.extend(excerpt);
        }
        Some(lines.join("\n"))
    }

    fn push_paragraph(result: &mut String, text: &str) {
        if !result.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str(text);
    }

    async fn mark_read_files(&self, cwd: &Path, analysis: &CommandAnalysis, stdout: &str) {
        let read_paths = extract_read_paths(analysis, stdout);
        for operand in read_paths {
            self.mark_read_path(cwd, &operand.path, operand.full_coverage)
                .await;
        }
    }

    async fn mark_read_path(&self, cwd: &Path, path: &str, full_coverage: bool) {
        if path.trim().is_empty() || path.contains('*') || path.contains('?') || path.contains('[')
        {
            return;
        }

        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };

        if let Ok(canonical) = tokio::fs::canonicalize(&path).await
            && canonical.starts_with(&self.canonical_project_root)
            && tokio::fs::metadata(&canonical)
                .await
                .map(|meta| meta.is_file())
                .unwrap_or(false)
        {
            // `head`/`tail`/`grep` only showed a window or the matching lines, so
            // they mark partial coverage — the read still satisfies `edit`
            // (content-addressed) but not a whole-file `write` (P4). `cat` stays
            // full.
            if full_coverage {
                self.read_tracker.mark_read(&canonical).await;
            } else {
                self.read_tracker.mark_read_partial(&canonical).await;
            }
        }
    }
}

/// Whether a confined command's failure output carries a signature of the
/// sandbox itself blocking it — an OS write denial or a denied-network error.
/// Only these should steer the model toward `escape_sandbox`; every other
/// non-zero exit (lint/test failure, missing path, non-sandbox auth error) is a
/// real failure that escaping cannot fix. Kept deliberately narrow: false
/// negatives just withhold a nudge (the tool schema still documents escape),
/// whereas the old always-nudge trained the model to escape on any failure.
///
/// Signatures span both backends: macOS Seatbelt surfaces write and network
/// denials as EPERM ("operation not permitted"); Linux Bubblewrap surfaces a
/// read-only bind as EROFS ("read-only file system") and other blocks as EACCES
/// ("permission denied"). The DNS/route strings catch denied network egress,
/// which fails before any connection when resolution or routing is walled off.
fn failure_looks_sandbox_related(body: &str) -> bool {
    const SIGNATURES: &[&str] = &[
        "operation not permitted",
        "read-only file system",
        "permission denied",
        "could not resolve host",
        "couldn't resolve host",
        "temporary failure in name resolution",
        "network is unreachable",
        "no route to host",
    ];
    let lower = body.to_ascii_lowercase();
    SIGNATURES.iter().any(|sig| lower.contains(sig))
}

/// Whether a requested sandbox escape is pointless because the command provably
/// stays inside the sandbox — every segment is a git operation that writes only
/// within the repository (`.git/` and the working tree), touching neither the
/// network nor any path outside the workspace. These succeed confined, so
/// prompting the user to step past the sandbox for them is pure friction
/// (observed: the model habitually sets `escape_sandbox` for `git add &&
/// git commit`).
///
/// Deliberately conservative — it returns `true` only when *every* segment is a
/// clearly workspace-only git subcommand with no write-redirect. Anything else
/// (a non-git segment, a network subcommand like `push`/`pull`/`fetch`/`clone`,
/// `config` which may be `--global`, `worktree`/`init` which take out-of-tree
/// paths, or any `>` redirect) keeps the escape prompt. Declining is safe by
/// construction: it only *keeps* confinement, never relaxes it.
fn escape_is_unnecessary(analysis: &CommandAnalysis) -> bool {
    // Git subcommands whose writes are confined to the repo: no network, no
    // `$HOME`, no out-of-tree path argument.
    const WORKSPACE_ONLY_GIT: &[&str] = &[
        "add",
        "commit",
        "stash",
        "restore",
        "reset",
        "mv",
        "rm",
        "branch",
        "tag",
        "checkout",
        "switch",
        "merge",
        "rebase",
        "cherry-pick",
        "revert",
        "am",
        "apply",
        "status",
        "diff",
        "log",
        "show",
        "blame",
        "notes",
    ];

    let segments = analysis.permission_commands();
    if segments.is_empty() {
        return false;
    }
    segments.iter().all(|segment| {
        let words: Vec<&str> = segment.split_whitespace().collect();
        // Program must be git (a bare name or a path to it).
        let Some(prog) = words.first() else {
            return false;
        };
        if prog.rsplit(['/', '\\']).next().unwrap_or(prog) != "git" {
            return false;
        }
        // A write-redirect could target outside the workspace; keep the prompt.
        if words.iter().any(|word| word.starts_with('>')) {
            return false;
        }
        // Reuse the risk classifier's git parser so global options that take an
        // argument (`-c cfg`, `--git-dir dir`) don't get mistaken for the
        // subcommand.
        matches!(
            crate::tool::risk::git_subcommand(&words),
            Some(sub) if WORKSPACE_ONLY_GIT.contains(&sub)
        )
    })
}

fn effective_timeout_secs(timeout: Option<u64>, run_in_background: bool) -> u64 {
    let default = if run_in_background {
        DEFAULT_BACKGROUND_TIMEOUT_SECS
    } else {
        DEFAULT_TIMEOUT_SECS
    };
    timeout.unwrap_or(default).min(MAX_TIMEOUT_SECS)
}

#[cfg(test)]
mod tests {
    use super::output::{MAX_OUTPUT_CHARS, SMOL_MAX_OUTPUT_CHARS};
    use super::policy::{GateVerdict, permission_gate};
    use super::*;
    use crate::config::{
        Config, ConfigSource, FailureBehavior, HookAction, HookDef, HookEvent, HookMatcher,
    };
    use crate::extension::status::ExtensionRegistry;
    use crate::interaction::{EscalationDecision, InteractionOutcome};
    use crate::output::OutputSink;
    use crate::permissions::PermissionMatchSource;
    use crate::permissions::{Permission, PermissionMatch};
    use crate::tool::EditTool;
    use crate::tool::RiskTier;
    use crate::tool::risk::ApprovalLevel;
    use crate::tool::test_utils::TestFixture;
    use crate::yolo::YoloMode;
    use serde_json::json;

    fn rendered_command_output(output: ToolOutput) -> String {
        match output {
            ToolOutput::Command { rendered, .. } => rendered,
            _ => panic!("Expected Command output"),
        }
    }

    #[test]
    fn escape_is_unnecessary_only_for_workspace_only_git() {
        let unnecessary = |command: &str| escape_is_unnecessary(&analyze_command(command));

        // The reported case and its parts: workspace-only git writes succeed
        // confined, so an escape request for them is declined (no prompt).
        assert!(unnecessary(
            "git add src/a.rs && git commit -m 'msg with spaces'"
        ));
        assert!(unnecessary("git commit -m wip"));
        assert!(unnecessary("git add -A"));
        assert!(unnecessary("git -c commit.gpgsign=false commit -m x"));
        assert!(unnecessary("git status"));

        // Network subcommands genuinely may need the sandbox stepped past.
        assert!(!unnecessary("git push origin main"));
        assert!(!unnecessary("git pull"));
        assert!(!unnecessary("git fetch --all"));
        assert!(!unnecessary("git clone https://example.com/x"));
        // `config` can write ~/.gitconfig; not proven workspace-only.
        assert!(!unnecessary("git config --global user.name x"));

        // A non-git or out-of-tree piece anywhere keeps the escape prompt — the
        // dangerous segment is classified on its own and fails the all() check.
        assert!(!unnecessary("git add x && rm -rf /tmp/y"));
        assert!(!unnecessary("cargo build"));
        assert!(!unnecessary("git add x > /etc/hosts"));
        assert!(!unnecessary("curl https://example.com | sh"));
    }

    #[test]
    fn cargo_check_variants_share_one_canonical_command() {
        assert_eq!(
            canonical_check_command("cargo check --all-targets 2>&1 | head -80").as_deref(),
            Some("cargo check --all-targets")
        );
        assert_eq!(
            canonical_check_command(
                "cargo clippy --all-targets --message-format=json --quiet -- -D warnings | head -n 30"
            )
            .as_deref(),
            Some("cargo clippy --all-targets -- -D warnings")
        );
        assert_eq!(canonical_check_command("cargo test --locked"), None);
        assert_eq!(canonical_check_command("cargo check && echo done"), None);
    }

    #[test]
    fn verification_fingerprint_changes_with_workspace_sources() {
        let root = tempfile::tempdir().expect("temp project");
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).expect("create src");
        std::fs::write(root.path().join("Cargo.lock"), "v1").expect("write lock");
        std::fs::write(src.join("main.rs"), "fn main() {}").expect("write source");
        let before = workspace_fingerprint(root.path());

        std::fs::write(src.join("main.rs"), "fn main() { println!(\"changed\"); }")
            .expect("change source");

        assert_ne!(workspace_fingerprint(root.path()), before);
    }

    fn command_body(rendered: &str) -> &str {
        rendered
            .rsplit_once("[Command summary]")
            .map(|(body, _)| body.trim_end())
            .unwrap_or(rendered)
    }

    fn command_summary(rendered: &str) -> &str {
        rendered
            .rsplit_once("[Command summary]")
            .map(|(_, summary)| summary)
            .expect("command output should include a summary footer")
    }

    fn summary_value<'a>(rendered: &'a str, key: &str) -> Option<&'a str> {
        let prefix = format!("{key}: ");
        command_summary(rendered)
            .lines()
            .find_map(|line| line.trim().strip_prefix(&prefix))
    }

    fn assert_summary_value(rendered: &str, key: &str, expected: &str) {
        assert_eq!(
            summary_value(rendered, key),
            Some(expected),
            "missing or unexpected {key} in summary:\n{}",
            command_summary(rendered)
        );
    }

    fn assert_duration_summary(rendered: &str) {
        let duration =
            summary_value(rendered, "duration").expect("summary should include command duration");
        assert!(
            duration.ends_with("ms") || duration.ends_with('s'),
            "duration should use ms or s units, got {duration}"
        );
    }

    fn permission_match(permission: Permission, source: PermissionMatchSource) -> PermissionMatch {
        PermissionMatch { permission, source }
    }

    #[test]
    fn permission_gate_covers_deny_allow_and_ask() {
        let plain = analyze_command("echo hi");
        let relative_redirect = analyze_command("echo x > Cargo.toml");
        let out_of_tree_redirect = analyze_command("echo x > ~/.bashrc");
        let destructive = analyze_command("git branch -D main");
        let high = analyze_command("git push origin main");

        // Deny refuses regardless of level.
        assert_eq!(
            permission_gate(
                permission_match(Permission::Deny, PermissionMatchSource::HardDeny),
                &plain,
                ApprovalLevel::Yolo,
            ),
            GateVerdict::Deny
        );

        // An explicit project/session allow records a user decision, so it
        // applies even when the ambient autonomy level would otherwise ask.
        assert!(matches!(
            permission_gate(
                permission_match(Permission::Allow, PermissionMatchSource::Project),
                &plain,
                ApprovalLevel::Ask,
            ),
            GateVerdict::AutoApproved {
                tier: RiskTier::ReadOnly,
                matched_allow_rule: true,
            }
        ));

        // A remembered allow covers mutations too, but never the destructive
        // floor below Yolo.
        assert!(matches!(
            permission_gate(
                permission_match(Permission::Allow, PermissionMatchSource::Project),
                &relative_redirect,
                ApprovalLevel::Conservative
            ),
            GateVerdict::AutoApproved {
                tier: RiskTier::Medium,
                matched_allow_rule: true,
            }
        ));
        assert!(matches!(
            permission_gate(
                permission_match(Permission::Allow, PermissionMatchSource::Project),
                &out_of_tree_redirect,
                ApprovalLevel::Ask,
            ),
            GateVerdict::AutoApproved {
                tier: RiskTier::High,
                matched_allow_rule: true,
            }
        ));

        // The destructive floor holds for an allow rule below yolo.
        assert_eq!(
            permission_gate(
                permission_match(Permission::Allow, PermissionMatchSource::Project),
                &destructive,
                ApprovalLevel::AutoAccept,
            ),
            GateVerdict::NeedsPrompt
        );

        // A persisted allow covers project binaries and other high-risk,
        // non-destructive commands without requiring a higher autonomy level.
        assert!(matches!(
            permission_gate(
                permission_match(Permission::Allow, PermissionMatchSource::Project),
                &high,
                ApprovalLevel::Ask,
            ),
            GateVerdict::AutoApproved {
                tier: RiskTier::High,
                matched_allow_rule: true,
            }
        ));
        // A built-in allow is only a default: it must not bypass the active
        // autonomy ceiling when its command has a high-risk effect.
        assert_eq!(
            permission_gate(
                permission_match(Permission::Allow, PermissionMatchSource::BuiltInDefault),
                &high,
                ApprovalLevel::Ask,
            ),
            GateVerdict::NeedsPrompt
        );

        // Ask: under the level's ceiling auto-approves (carrying the tier for
        // the log line), over it prompts.
        assert!(matches!(
            permission_gate(
                permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                &plain,
                ApprovalLevel::Balanced,
            ),
            GateVerdict::AutoApproved {
                matched_allow_rule: false,
                ..
            }
        ));
        assert_eq!(
            permission_gate(
                permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                &plain,
                ApprovalLevel::Ask,
            ),
            GateVerdict::NeedsPrompt
        );
    }

    fn summary_with_exit(exit_code: Option<i32>) -> CommandSummary {
        CommandSummary {
            exit_code,
            signal: None,
            timed_out: false,
            timeout_secs: 30,
            duration: std::time::Duration::from_millis(5),
            stdout_bytes: 3,
            stderr_bytes: 0,
            combined_output_chars: 3,
            saved_output: None,
            last_output_lines: vec!["out".to_string()],
        }
    }

    #[test]
    fn sandbox_denial_signatures_are_detected_case_insensitively() {
        // Both backends' denial shapes, plus denied-network errors.
        for body in [
            "touch: /etc/x: Operation not permitted",
            "error: Read-only file system (os error 30)",
            "fatal: could not create work tree dir: Permission denied",
            "fatal: unable to access 'https://…': Could not resolve host: github.com",
            "curl: (7) Couldn't resolve host 'example.com'",
            "ping: connect: Network is unreachable",
        ] {
            assert!(
                failure_looks_sandbox_related(body),
                "should flag as sandbox-shaped: {body}"
            );
        }

        // Ordinary failures escaping cannot fix — no nudge.
        for body in [
            "error[E0433]: failed to resolve: use of undeclared crate",
            "test result: FAILED. 1 passed; 2 failed",
            "fatal: pathspec 'nope.rs' did not match any files",
            "clippy: unused variable `x`",
        ] {
            assert!(
                !failure_looks_sandbox_related(body),
                "should NOT flag as sandbox-shaped: {body}"
            );
        }
    }

    #[test]
    fn finalize_foreground_nudges_escape_only_on_sandbox_shaped_failure() {
        let failed = summary_with_exit(Some(1));

        // A confined failure whose output shows an OS write denial: nudge.
        let nudged = BashTool::finalize_foreground(
            "touch /etc/blocked",
            "touch: /etc/blocked: Operation not permitted",
            Some(1),
            false,
            true,
            EscapeApproval::None,
            false,
            &failed,
        );
        assert!(nudged.contains("escape_sandbox=true"), "{nudged}");
        assert!(nudged.contains("[Command summary]"), "{nudged}");

        // A confined failure that is NOT the sandbox — a `git commit` whose
        // pre-commit hook's clippy failed — must not steer toward escape, which
        // would waste a user approval on a command the sandbox never blocked.
        let lint_failure = BashTool::finalize_foreground(
            "git commit -m x",
            "error: unused variable `foo`\nerror: could not compile due to previous error",
            Some(1),
            false,
            true,
            EscapeApproval::None,
            false,
            &failed,
        );
        assert!(
            !lint_failure.contains("escape_sandbox=true"),
            "{lint_failure}"
        );

        // No nudge on success, outside confinement, or when the run already
        // escaped the sandbox — even with a denial-shaped body.
        let succeeded = BashTool::finalize_foreground(
            "true",
            "out",
            Some(0),
            false,
            true,
            EscapeApproval::None,
            false,
            &summary_with_exit(Some(0)),
        );
        assert!(!succeeded.contains("escape_sandbox=true"), "{succeeded}");

        let unconfined = BashTool::finalize_foreground(
            "touch /etc/blocked",
            "touch: /etc/blocked: Operation not permitted",
            Some(1),
            false,
            false,
            EscapeApproval::None,
            false,
            &failed,
        );
        assert!(!unconfined.contains("escape_sandbox=true"), "{unconfined}");

        let escaped = BashTool::finalize_foreground(
            "touch /etc/blocked",
            "touch: /etc/blocked: Operation not permitted",
            Some(1),
            false,
            true,
            EscapeApproval::Once,
            false,
            &failed,
        );
        assert!(!escaped.contains("escape_sandbox=true"), "{escaped}");
    }

    #[test]
    fn declined_escape_failure_promises_the_prompt_on_retry() {
        // When the escape was auto-declined and the confined run then failed
        // sandbox-shaped, the nudge must say the shortcut is lifted — otherwise
        // the guidance ("retry with escape_sandbox=true") loops the model into
        // another silent decline.
        let failed = summary_with_exit(Some(1));
        let declined = BashTool::finalize_foreground(
            "git commit -m x",
            "mktemp: mkstemp failed on /var/folders/x/T/tmp.abc: Operation not permitted",
            Some(1),
            false,
            true,
            EscapeApproval::None,
            true,
            &failed,
        );
        assert!(declined.contains("declined"), "{declined}");
        assert!(declined.contains("escape_sandbox=true"), "{declined}");
        assert!(declined.contains("approval prompt"), "{declined}");
    }

    #[test]
    fn missing_common_command_gets_a_concrete_fallback() {
        let rendered = BashTool::finalize_foreground(
            "rg needle src",
            "/bin/sh: rg: command not found",
            Some(127),
            false,
            false,
            EscapeApproval::None,
            false,
            &summary_with_exit(Some(127)),
        );

        assert!(
            rendered.contains("retry the search with `grep -R`"),
            "{rendered}"
        );
    }

    #[test]
    fn confined_failures_lift_the_decline_shortcut_per_exact_command() {
        use super::session::ConfinedFailures;
        let failures = ConfinedFailures::default();
        let cwd = std::path::Path::new("/repo");

        assert!(!failures.contains(cwd, "git commit -m x"));
        failures.record(cwd, "git commit -m x");
        assert!(failures.contains(cwd, "git commit -m x"));
        // Exact-match scoping, mirroring escape grants: neither a different
        // command nor a different cwd inherits the lifted shortcut.
        assert!(!failures.contains(cwd, "git commit -m y"));
        assert!(!failures.contains(std::path::Path::new("/other"), "git commit -m x"));
    }

    #[derive(Default)]
    struct CaptureToolOutputSink {
        outputs: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl CaptureToolOutputSink {
        fn outputs(&self) -> Vec<(String, String)> {
            self.outputs
                .lock()
                .expect("capture sink mutex should not be poisoned")
                .clone()
        }
    }

    impl OutputSink for CaptureToolOutputSink {
        fn tool_output(&self, id: &str, output: &str) {
            self.outputs
                .lock()
                .expect("capture sink mutex should not be poisoned")
                .push((id.to_string(), output.to_string()));
        }
    }

    /// An *active* sandbox (`is_active() == true`) on any OS — `SeatbeltExec` is
    /// always "available"; actual confinement is macOS-only.
    fn active_sandbox(root: &Path) -> CommandSandbox {
        let sandbox = CommandSandbox::new(crate::sandbox::SandboxBackend::test_seatbelt(), root);
        sandbox.set_enabled(true);
        sandbox
    }

    /// A bash tool with yolo enabled so the *permission* gate is skipped — these
    /// tests isolate the *sandbox-escape* gate, which runs independently of yolo.
    fn escape_tool(
        fixture: &TestFixture,
        service: Arc<InteractionService>,
        sandbox: CommandSandbox,
    ) -> BashTool {
        let yolo = YoloMode::new();
        yolo.set_enabled(true);
        BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
            Arc::new(BackgroundTaskRegistry::new()),
            yolo,
            sandbox,
        )
    }

    /// Spawn a background responder that replies `decision` to every sandbox-escape
    /// request, counting how many it saw.
    fn spawn_escape_responder(
        service: &Arc<InteractionService>,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::interaction::InteractionRequest>,
        decision: EscalationDecision,
    ) -> Arc<std::sync::atomic::AtomicUsize> {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let responder = service.clone();
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                if let crate::interaction::InteractionRequest::SandboxEscalation {
                    request_id,
                    ..
                } = request
                {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = responder
                        .respond(
                            request_id,
                            InteractionOutcome::SandboxEscalation(decision.clone()),
                        )
                        .await;
                }
            }
        });
        count
    }

    #[tokio::test]
    async fn escape_with_active_sandbox_noninteractive_bails() {
        let fixture = TestFixture::new();
        let service = Arc::new(InteractionService::noninteractive());
        let tool = escape_tool(&fixture, service, active_sandbox(&fixture.project_root));

        let err = tool
            .execute(json!({"command": "echo hi", "escape_sandbox": true}))
            .await
            .expect_err(
                "escape with an active sandbox must prompt, failing closed when noninteractive",
            );
        assert!(
            err.to_string().contains("noninteractive"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn escape_with_inactive_sandbox_runs_without_prompt() {
        // Sandbox off -> escape is moot; the command runs unconfined with no
        // prompt even noninteractively (the yolo / no-backend path).
        let fixture = TestFixture::new();
        let service = Arc::new(InteractionService::noninteractive());
        let tool = escape_tool(&fixture, service, CommandSandbox::disabled());

        let output = tool
            .execute(json!({"command": "echo moot", "escape_sandbox": true}))
            .await
            .expect("an inactive sandbox must not prompt for an escape");
        assert!(rendered_command_output(output).contains("moot"));
    }

    #[tokio::test]
    async fn escape_approved_runs_unconfined() {
        let fixture = TestFixture::new();
        let (service, rx) = InteractionService::new();
        let service = Arc::new(service);
        spawn_escape_responder(&service, rx, EscalationDecision::AllowOnce);
        let tool = escape_tool(&fixture, service, active_sandbox(&fixture.project_root));

        let output = tool
            .execute(json!({"command": "echo escaped-ok", "escape_sandbox": true}))
            .await
            .expect("an approved escape should run");
        assert!(rendered_command_output(output).contains("escaped-ok"));
    }

    #[tokio::test]
    async fn command_and_escape_share_one_sandbox_warning_prompt() {
        let fixture = TestFixture::new();
        let (service, mut rx) = InteractionService::new();
        let service = Arc::new(service);
        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service.clone(),
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::new(),
            active_sandbox(&fixture.project_root),
        );
        let handle = tokio::spawn(async move {
            tool.execute(json!({
                "command": "echo combined-approval",
                "escape_sandbox": true
            }))
            .await
        });

        let request = rx.recv().await.expect("combined approval request");
        let crate::interaction::InteractionRequest::SandboxEscalation {
            request_id, kind, ..
        } = request
        else {
            panic!("command and escape must not emit a separate permission prompt");
        };
        assert_eq!(
            kind,
            crate::interaction::SandboxEscalationKind::CommandAndSandbox
        );
        service
            .respond(
                request_id,
                InteractionOutcome::SandboxEscalation(EscalationDecision::AllowOnce),
            )
            .await
            .expect("combined decision should be delivered");

        let output = handle
            .await
            .expect("command task should join")
            .expect("combined approval should run the command");
        assert!(rendered_command_output(output).contains("combined-approval"));
        assert!(
            rx.try_recv().is_err(),
            "combined approval must not enqueue a second sandbox prompt"
        );
    }

    #[tokio::test]
    async fn auto_accept_escapes_only_an_evidenced_safe_retry() {
        let fixture = TestFixture::new();
        let (service, mut rx) = InteractionService::new();
        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            Arc::new(service),
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::with_level(ApprovalLevel::AutoAccept),
            active_sandbox(&fixture.project_root),
        );
        let command = "echo automatic-safe-retry";
        tool.confined_failures
            .record(&fixture.project_root, command);

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tool.execute(json!({
                "command": command,
                "escape_sandbox": true
            })),
        )
        .await
        .expect("safe retry should not wait for a prompt")
        .expect("safe retry should run");
        let rendered = rendered_command_output(output);
        assert!(rendered.contains("automatic-safe-retry"));
        assert!(
            rx.try_recv().is_err(),
            "an evidenced safe retry at auto-accept should not prompt"
        );

        let direct = analyze_command("echo first-attempt");
        assert!(
            tool.escape_requires_prompt(&fixture.project_root, "echo first-attempt", &direct),
            "a first-attempt escape must still prompt"
        );
        let high_risk = analyze_command("curl https://example.com");
        tool.confined_failures
            .record(&fixture.project_root, "curl https://example.com");
        assert!(
            tool.escape_requires_prompt(
                &fixture.project_root,
                "curl https://example.com",
                &high_risk
            ),
            "high-risk retries must still prompt"
        );
    }

    #[tokio::test]
    async fn escape_session_grant_skips_second_prompt() {
        let fixture = TestFixture::new();
        let (service, rx) = InteractionService::new();
        let service = Arc::new(service);
        let requests = spawn_escape_responder(&service, rx, EscalationDecision::AllowForSession);
        let tool = escape_tool(&fixture, service, active_sandbox(&fixture.project_root));

        for _ in 0..2 {
            tool.execute(json!({"command": "echo same-cmd", "escape_sandbox": true}))
                .await
                .expect("escape should run both times");
        }
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the session grant should have skipped the second prompt"
        );
    }

    #[tokio::test]
    async fn escape_grant_does_not_leak_across_cwd() {
        // A "for session" grant is scoped to the (cwd, command) pair: the SAME
        // command in a DIFFERENT directory must re-prompt, so one approval can't
        // silently authorize an unconfined run elsewhere (matters at yolo, where
        // the working-directory clamp is off).
        let fixture = TestFixture::new();
        std::fs::create_dir_all(fixture.project_root.join("dir_a")).unwrap();
        std::fs::create_dir_all(fixture.project_root.join("dir_b")).unwrap();
        let (service, rx) = InteractionService::new();
        let service = Arc::new(service);
        let requests = spawn_escape_responder(&service, rx, EscalationDecision::AllowForSession);
        let tool = escape_tool(&fixture, service, active_sandbox(&fixture.project_root));

        tool.execute(json!({"command": "echo x", "escape_sandbox": true, "workdir": "dir_a"}))
            .await
            .expect("escape in dir_a should run");
        tool.execute(json!({"command": "echo x", "escape_sandbox": true, "workdir": "dir_b"}))
            .await
            .expect("escape in dir_b should run");

        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a session grant must not carry across a different cwd"
        );
    }

    #[tokio::test]
    async fn escape_with_background_is_rejected() {
        let fixture = TestFixture::new();
        // Rejected even with the sandbox OFF (default): it's an argument-shape
        // contradiction, not a runtime-state decision.
        let service = Arc::new(InteractionService::noninteractive());
        let tool = escape_tool(&fixture, service, CommandSandbox::disabled());

        let err = tool
            .execute(
                json!({"command": "echo hi", "escape_sandbox": true, "run_in_background": true}),
            )
            .await
            .expect_err("escape + background must be rejected in v1");
        assert!(
            err.to_string().contains("not supported for background"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn foreground_command_streams_live_output_before_exit() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let sink = Arc::new(CaptureToolOutputSink::default());
        let shared_sink: crate::output::SharedSink = sink.clone();

        let handle = tokio::spawn(async move {
            tool.execute_with_context(
                json!({
                    "command": "printf 'warning: live output\\n'; sleep 0.4",
                    "timeout": 2
                }),
                ToolExecutionContext::new("call-live".to_string(), shared_sink),
            )
            .await
        });

        let mut saw_live_output = false;
        for _ in 0..20 {
            if sink
                .outputs()
                .iter()
                .any(|(id, output)| id == "call-live" && output.contains("warning: live output"))
            {
                saw_live_output = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            saw_live_output,
            "foreground bash should stream output before the command exits"
        );

        let output = handle
            .await
            .expect("command task should join")
            .expect("command should run");
        assert!(rendered_command_output(output).contains("warning: live output"));
    }

    #[tokio::test]
    async fn truncated_live_output_keeps_recent_tail() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let sink = Arc::new(CaptureToolOutputSink::default());
        let shared_sink: crate::output::SharedSink = sink.clone();

        let output = tool
            .execute_with_context(
                json!({
                    "command": "yes x | head -n 7000; printf 'latest live line\\n'",
                    "timeout": 5
                }),
                ToolExecutionContext::new("call-live".to_string(), shared_sink),
            )
            .await
            .expect("command should run");

        assert!(rendered_command_output(output).contains("latest live line"));
        let outputs = sink.outputs();
        assert!(
            outputs.iter().any(|(id, output)| {
                id == "call-live"
                    && output.contains("latest live line")
                    && output.contains("middle chars omitted")
            }),
            "live outputs should retain the recent tail: {outputs:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn confined_write_denial_nudges_toward_escape() {
        // Real confinement (macOS Seatbelt) so the command is actually
        // `confined`. A write outside the project root is blocked with
        // "Operation not permitted", which is a genuine sandbox denial, so the
        // retry nudge is appended.
        let fixture = TestFixture::new();
        let service = Arc::new(InteractionService::noninteractive());
        let tool = escape_tool(&fixture, service, active_sandbox(&fixture.project_root));

        let output = tool
            .execute(json!({"command": "touch /etc/bonsai-sandbox-denial-test"}))
            .await
            .expect("a non-zero exit is not a tool error");
        assert!(rendered_command_output(output).contains("escape_sandbox=true"));
    }

    // The escape-only-on-sandbox-signature behaviour is covered by the unit
    // test `finalize_foreground_nudges_escape_only_on_sandbox_shaped_failure`
    // above. An integration test through the full `execute` path is unreliable
    // on macOS: `sandbox-exec` often refuses to apply a nested profile from
    // within a test-process seatbelt (exit 71, "sandbox_apply: Operation not
    // permitted"), and that error itself contains the denial signature we are
    // trying to avoid, making the test self-defeating.

    #[tokio::test]
    async fn large_output_streams_to_a_spool_file_with_full_contents() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        // Emit well past MAX_OUTPUT_CHARS so the streaming path must spool to
        // disk instead of holding everything in memory.
        let lines = 5_000;
        let result = tool
            .execute(json!({
                "command": format!(
                    "for i in $(seq 1 {lines}); do echo \"line-$i-padding-padding-padding-padding\"; done"
                )
            }))
            .await
            .unwrap();

        let (rendered, truncation) = match result {
            ToolOutput::Command {
                rendered,
                truncation,
                ..
            } => (rendered, truncation),
            other => panic!("expected Command output, got {other:?}"),
        };

        let truncation = truncation.expect("a large output should spool to a file");
        assert!(
            rendered.contains("[Output truncated:"),
            "rendered should note truncation: {rendered}"
        );
        assert!(truncation.total_chars > MAX_OUTPUT_CHARS);

        // The artifact holds the *complete* output (head + tail), not just the
        // in-memory preview — that's the whole point of spooling.
        let path = fixture
            .project_root
            .canonicalize()
            .unwrap()
            .join(&truncation.path);
        let spooled = std::fs::read_to_string(&path).expect("spool file should exist");
        assert!(spooled.contains("line-1-"), "spool should hold the head");
        assert!(
            spooled.contains(&format!("line-{lines}-")),
            "spool should hold the tail"
        );
        assert_eq!(
            spooled.chars().count(),
            truncation.total_chars,
            "the spooled file should contain the full output"
        );
    }

    #[tokio::test]
    async fn smol_output_budget_truncates_earlier_and_points_to_read() {
        let fixture = TestFixture::new();
        let normal = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::new(),
            CommandSandbox::disabled(),
        );
        let tool = normal.with_shared_session_and_output_budget(BashOutputBudget::smol());

        let output = tool
            .execute(json!({
                "command": "yes x | head -c 7000",
                "timeout": 5
            }))
            .await
            .expect("command should run");

        let ToolOutput::Command {
            rendered,
            truncation,
            ..
        } = output
        else {
            panic!("expected command output");
        };
        let truncation = truncation.expect("SMOL output should truncate below the normal cap");
        assert!(truncation.total_chars > SMOL_MAX_OUTPUT_CHARS);
        assert!(rendered.contains("[Output truncated:"));
        // SMOL carries the read tool, so its recovery hint matches the normal
        // budget's instead of steering back into bash sed/tail incantations.
        assert!(rendered.contains("Use Read tool"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_timeout_kills_spawned_children() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        // The shell forks a long sleep, prints its pid, then blocks. On timeout
        // the whole process group must be killed, taking the backgrounded sleep
        // with it rather than orphaning it.
        let result = tool
            .execute(json!({
                "command": "sleep 30 & echo $!; sleep 30",
                "timeout": 1
            }))
            .await
            .unwrap();
        let rendered = rendered_command_output(result);
        assert_summary_value(&rendered, "timed_out", "true");

        let pid: i32 = command_body(&rendered)
            .lines()
            .find_map(|line| line.trim().parse().ok())
            .expect("child pid should be present in command output");

        // Poll briefly for the SIGTERM/SIGKILL to take effect.
        let alive = || nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        let mut survived = true;
        for _ in 0..50 {
            if !alive() {
                survived = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!survived, "spawned child {pid} survived the timeout");
    }

    #[test]
    fn timeout_defaults_preserve_foreground_and_background_semantics() {
        assert_eq!(effective_timeout_secs(None, false), DEFAULT_TIMEOUT_SECS);
        assert_eq!(
            effective_timeout_secs(None, true),
            DEFAULT_BACKGROUND_TIMEOUT_SECS
        );
        assert_eq!(effective_timeout_secs(Some(999), false), MAX_TIMEOUT_SECS);
        assert_eq!(effective_timeout_secs(Some(999), true), MAX_TIMEOUT_SECS);
    }

    #[tokio::test]
    async fn successful_command_output_includes_summary_footer() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        let rendered = rendered_command_output(
            tool.execute(json!({"command": "printf 'alpha\\nbeta\\n'"}))
                .await
                .unwrap(),
        );

        assert!(command_body(&rendered).contains("alpha\nbeta"));
        assert_summary_value(&rendered, "command", "printf 'alpha\\nbeta\\n'");
        assert_summary_value(&rendered, "exit_code", "0");
        assert_summary_value(&rendered, "timed_out", "false");
        assert_summary_value(&rendered, "stdout_bytes", "11");
        assert_summary_value(&rendered, "stderr_bytes", "0");
        assert_summary_value(&rendered, "combined_output_chars", "11");
        assert_duration_summary(&rendered);
        assert!(
            command_summary(&rendered).contains("last_output:\nalpha\nbeta"),
            "summary should include useful tail lines:\n{}",
            command_summary(&rendered)
        );
    }

    #[tokio::test]
    async fn cargo_check_cache_reuses_variants_and_invalidates_after_source_change() {
        let fixture = TestFixture::new();
        std::fs::create_dir_all(fixture.project_root.join("src")).unwrap();
        std::fs::write(
            fixture.project_root.join("Cargo.toml"),
            "[package]\nname = \"cache-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            fixture.project_root.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\nversion = 4\n",
        )
        .unwrap();
        let source = fixture.project_root.join("src/lib.rs");
        std::fs::write(&source, "pub fn value() -> u8 { 1 }\n").unwrap();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        let first = rendered_command_output(
            tool.execute(json!({"command": "cargo check --offline 2>&1 | head -30"}))
                .await
                .unwrap(),
        );
        assert!(first.contains("No diagnostics found."), "{first}");

        let cached = rendered_command_output(
            tool.execute(json!({"command": "cargo check --offline | head -80"}))
                .await
                .unwrap(),
        );
        assert!(cached.contains("verification cache hit"), "{cached}");

        std::fs::write(&source, "pub fn value() -> u8 { missing }\n").unwrap();
        let invalidated = rendered_command_output(
            tool.execute(json!({"command": "cargo check --offline"}))
                .await
                .unwrap(),
        );
        assert!(
            !invalidated.contains("verification cache hit"),
            "{invalidated}"
        );
        assert!(invalidated.contains("src/lib.rs:1:"), "{invalidated}");
    }

    #[tokio::test]
    async fn successful_no_output_command_still_includes_summary_footer() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        let rendered =
            rendered_command_output(tool.execute(json!({"command": "true"})).await.unwrap());

        assert!(command_body(&rendered).contains("Command completed successfully"));
        assert_summary_value(&rendered, "exit_code", "0");
        assert_summary_value(&rendered, "stdout_bytes", "0");
        assert_summary_value(&rendered, "stderr_bytes", "0");
        assert!(
            command_summary(&rendered).contains("last_output:\n(no output)"),
            "summary should report empty output:\n{}",
            command_summary(&rendered)
        );
    }

    #[tokio::test]
    async fn nonzero_command_summary_includes_exit_code() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        let rendered =
            rendered_command_output(tool.execute(json!({"command": "exit 42"})).await.unwrap());

        assert_summary_value(&rendered, "exit_code", "42");
        assert_summary_value(&rendered, "timed_out", "false");
    }

    #[tokio::test]
    async fn nonzero_command_prepends_failure_summary_with_diagnostic_excerpt() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let command = concat!(
            "printf '   Compiling bonsai\\n",
            "error[E0609]: no field `tempdir` on type `TestFixture`\\n",
            " --> src/tool/bash.rs:2448:31\\n",
            "help: a field with a similar name exists\\n",
            "2448 | fixture.temp_dir.path()\\n' >&2; exit 101"
        );

        let rendered =
            rendered_command_output(tool.execute(json!({"command": command})).await.unwrap());
        let body = command_body(&rendered);

        assert!(body.starts_with("[Command failure summary]"), "{body}");
        assert!(body.contains("error[E0609]"), "{body}");
        assert!(body.contains("fixture.temp_dir.path()"), "{body}");
        assert_summary_value(&rendered, "exit_code", "101");
    }

    #[tokio::test]
    async fn stderr_output_increments_stderr_bytes() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        let rendered = rendered_command_output(
            tool.execute(json!({"command": "printf err >&2"}))
                .await
                .unwrap(),
        );

        assert_summary_value(&rendered, "stdout_bytes", "0");
        assert_summary_value(&rendered, "stderr_bytes", "3");
        assert!(
            command_summary(&rendered).contains("last_output:\nerr"),
            "stderr should contribute to combined tail lines:\n{}",
            command_summary(&rendered)
        );
    }

    #[tokio::test]
    async fn timeout_summary_includes_timeout_metadata() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        let rendered = rendered_command_output(
            tool.execute(json!({"command": "sleep 10", "timeout": 1}))
                .await
                .unwrap(),
        );

        assert_summary_value(&rendered, "exit_code", "none");
        assert_summary_value(&rendered, "timed_out", "true");
        assert_summary_value(&rendered, "timeout_seconds", "1");
        assert_duration_summary(&rendered);
    }

    #[tokio::test]
    async fn truncated_output_summary_points_to_saved_output_and_tail() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let lines = 5_000;

        let result = tool
            .execute(json!({
                "command": format!(
                    "for i in $(seq 1 {lines}); do printf 'summary-tail-%04d-padding-padding-padding-padding\\n' \"$i\"; done"
                )
            }))
            .await
            .unwrap();
        let rendered = rendered_command_output(result);

        let saved_output = summary_value(&rendered, "saved_output")
            .expect("truncated command should include saved_output");
        assert!(saved_output.starts_with(".bonsai/tool-output/"));
        let combined_chars = summary_value(&rendered, "combined_output_chars")
            .and_then(|value| value.parse::<usize>().ok())
            .expect("summary should include total combined chars");
        assert!(combined_chars > MAX_OUTPUT_CHARS);
        assert!(
            command_summary(&rendered).contains(&format!("summary-tail-{lines:04}")),
            "summary should include tail lines from the full output:\n{}",
            command_summary(&rendered)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_terminated_command_summary_includes_signal() {
        let fixture = TestFixture::new();
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);
        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            Arc::new(crate::interaction::InteractionService::noninteractive()),
            Arc::new(BackgroundTaskRegistry::new()),
            yolo_mode,
            CommandSandbox::disabled(),
        );

        let rendered = rendered_command_output(
            tool.execute(json!({"command": "kill -9 $$"}))
                .await
                .unwrap(),
        );

        assert_summary_value(&rendered, "exit_code", "none");
        assert_summary_value(&rendered, "signal", "9");
    }

    #[tokio::test]
    async fn test_bash_simple_command() {
        let fixture = TestFixture::new();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let result = tool
            .execute(json!({
                "command": "echo 'Hello, world!'"
            }))
            .await
            .unwrap();

        let output = rendered_command_output(result);

        assert!(output.contains("Hello, world!"));
    }

    #[tokio::test]
    async fn bash_defaults_to_project_root_workdir() {
        let fixture = TestFixture::new();
        let expected = fixture.project_root.canonicalize().unwrap();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let result = tool.execute(json!({"command": "pwd"})).await.unwrap();

        let output = rendered_command_output(result);

        assert_eq!(command_body(&output).trim(), expected.display().to_string());
    }

    #[tokio::test]
    async fn redundant_project_root_cd_is_removed_before_auto_approval() {
        let fixture = TestFixture::new();
        fixture.create_file(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        fixture.create_file("src/lib.rs", "pub fn fixture() {}\n");
        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            Arc::new(crate::interaction::InteractionService::noninteractive()),
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::with_level(ApprovalLevel::AutoAccept),
            CommandSandbox::disabled(),
        );
        let command = format!("cd {} && cargo fmt", fixture.project_root.display());

        let output =
            rendered_command_output(tool.execute(json!({"command": command})).await.unwrap());

        assert_summary_value(&output, "command", "cargo fmt");
        assert_summary_value(&output, "exit_code", "0");
    }

    #[tokio::test]
    async fn leading_cd_to_a_different_directory_is_not_removed() {
        let fixture = TestFixture::new();
        let subdir = fixture.project_root.join("subdir");
        tokio::fs::create_dir(&subdir).await.unwrap();
        let expected = fixture.project_root.canonicalize().unwrap();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        let output = rendered_command_output(
            tool.execute(json!({
                "command": format!("cd {} && pwd", expected.display()),
                "workdir": "subdir"
            }))
            .await
            .unwrap(),
        );

        assert_eq!(command_body(&output).trim(), expected.display().to_string());
        assert!(
            summary_value(&output, "command").is_some_and(|command| command.starts_with("cd ")),
            "meaningful cd should remain visible in the command summary: {output}"
        );
    }

    #[tokio::test]
    async fn smol_bash_variant_shares_persistent_cwd() {
        let fixture = TestFixture::new();
        let subdir = fixture.project_root.join("subdir");
        tokio::fs::create_dir(&subdir).await.unwrap();
        let expected = subdir.canonicalize().unwrap();

        let normal = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        normal
            .execute(json!({"command": "cd subdir"}))
            .await
            .unwrap();
        let smol = normal.with_shared_session_and_output_budget(BashOutputBudget::smol());

        let output =
            rendered_command_output(smol.execute(json!({"command": "pwd"})).await.unwrap());

        assert_eq!(command_body(&output).trim(), expected.display().to_string());
    }

    #[tokio::test]
    async fn bash_schema_bounds_timeout_and_is_closed() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let schema = tool.parameters_schema();

        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(false)
        );
        assert_eq!(schema["properties"]["timeout"]["minimum"], 1);
        assert_eq!(schema["properties"]["timeout"]["maximum"], MAX_TIMEOUT_SECS);
        assert!(schema["properties"]["interactive"].is_object());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interactive_bash_starts_a_shared_pty_terminal() {
        let fixture = TestFixture::new();
        let sandbox = CommandSandbox::disabled();
        let terminals = Arc::new(TerminalRegistry::with_sandbox(sandbox.clone()));
        let tool = BashTool::from_runtime(BashRuntimeDeps::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
            Arc::new(BackgroundTaskRegistry::new()),
            terminals.clone(),
            BashExecutionPolicy::new(
                YoloMode::new(),
                sandbox,
                Arc::new(crate::hooks::HookEngine::disabled()),
                AuthorizationLedger::disabled(),
            ),
        ));

        let output = tool
            .execute(json!({"command": "printf interactive-ok", "interactive": true}))
            .await
            .expect("interactive Bash should start");
        assert!(output.rendered_summary().contains("pty-1"));

        let finished = terminals
            .wait_for_terminal("pty-1", std::time::Duration::from_secs(5))
            .await
            .expect("shared terminal should remain inspectable");
        assert_eq!(finished.status, crate::terminal::TerminalStatus::Succeeded);
        assert!(finished.tail.contains("interactive-ok"));
        assert_eq!(finished.timeout_secs, DEFAULT_BACKGROUND_TIMEOUT_SECS);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interactive_bash_does_not_allocate_a_pty_when_pre_bash_vetoes() {
        let fixture = TestFixture::new();
        let config = Config {
            hooks: vec![(
                HookDef {
                    name: "block-interactive".to_string(),
                    event: HookEvent::PreBash,
                    matcher: HookMatcher::default(),
                    action: HookAction::Shell {
                        command: "printf 'interactive blocked' >&2; exit 2".to_string(),
                    },
                    timeout_secs: 5,
                    blocking: true,
                    on_failure: FailureBehavior::Warn,
                    capabilities: Vec::new(),
                    enabled: true,
                },
                ConfigSource::Global,
            )],
            ..Config::default()
        };
        let hooks = Arc::new(
            crate::hooks::HookEngine::build(
                &config,
                fixture.project_root.clone(),
                crate::permissions::PermissionManager::memory_only_hooks(),
                Arc::new(crate::interaction::InteractionService::noninteractive()),
                Arc::new(ExtensionRegistry::new()),
                None,
            )
            .await,
        );
        let sandbox = CommandSandbox::disabled();
        let terminals = Arc::new(TerminalRegistry::with_sandbox(sandbox.clone()));
        let tool = BashTool::from_runtime(BashRuntimeDeps::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
            Arc::new(BackgroundTaskRegistry::new()),
            terminals.clone(),
            BashExecutionPolicy::new(
                YoloMode::new(),
                sandbox,
                hooks,
                AuthorizationLedger::disabled(),
            ),
        ));

        let error = tool
            .execute(json!({"command": "printf should-not-run", "interactive": true}))
            .await
            .expect_err("blocking PreBash hook should veto interactive launch");

        assert!(error.to_string().contains("interactive blocked"));
        assert!(terminals.list().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interactive_bash_does_not_allocate_a_pty_before_permission() {
        let fixture = TestFixture::new();
        let sandbox = CommandSandbox::disabled();
        let terminals = Arc::new(TerminalRegistry::with_sandbox(sandbox.clone()));
        let tool = BashTool::from_runtime(BashRuntimeDeps::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            Arc::new(crate::interaction::InteractionService::noninteractive()),
            Arc::new(BackgroundTaskRegistry::new()),
            terminals.clone(),
            BashExecutionPolicy::new(
                YoloMode::new(),
                sandbox,
                Arc::new(crate::hooks::HookEngine::disabled()),
                AuthorizationLedger::disabled(),
            ),
        ));

        let error = tool
            .execute(json!({"command": "git reset --hard HEAD", "interactive": true}))
            .await
            .expect_err("interactive command should require permission before launch");

        assert!(
            error.to_string().contains("noninteractive mode")
                || error.to_string().contains("Command not allowed")
        );
        assert!(terminals.list().await.is_empty());
    }

    #[tokio::test]
    async fn interactive_bash_rejects_incompatible_modes_before_execution() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );

        for incompatible in ["parallel", "run_in_background", "escape_sandbox"] {
            let mut args = json!({"command": "printf should-not-run", "interactive": true});
            args[incompatible] = serde_json::Value::Bool(true);
            let error = tool
                .execute(args)
                .await
                .expect_err("incompatible interactive mode should fail");
            assert!(
                error
                    .to_string()
                    .contains("interactive mode cannot be combined"),
                "unexpected error for {incompatible}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn test_bash_with_timeout() {
        let fixture = TestFixture::new();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let result = tool
            .execute(json!({
                "command": "sleep 0.1",
                "timeout": 1
            }))
            .await
            .unwrap();

        let output = rendered_command_output(result);

        assert!(output.contains("Command completed successfully"));
    }

    #[tokio::test]
    async fn test_bash_starts_background_task_immediately() {
        let fixture = TestFixture::new();
        let registry = Arc::new(BackgroundTaskRegistry::new());

        let tool = BashTool::with_background_tasks(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
            registry.clone(),
        );
        let result = tool
            .execute(json!({
                "command": "printf background",
                "run_in_background": true
            }))
            .await
            .unwrap();

        let task_id = match result {
            ToolOutput::BackgroundTaskStarted { task_id, message } => {
                assert!(message.contains("Started background task"));
                task_id
            }
            _ => panic!("Expected background task output"),
        };

        let task = registry
            .wait_for_task(&task_id, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(task.timeout_secs, DEFAULT_BACKGROUND_TIMEOUT_SECS);
        assert!(task.tail.contains("background"), "tail: {}", task.tail);
    }

    #[tokio::test]
    async fn test_bash_background_timeout_is_capped() {
        let fixture = TestFixture::new();
        let registry = Arc::new(BackgroundTaskRegistry::new());

        let tool = BashTool::with_background_tasks(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
            registry.clone(),
        );
        let result = tool
            .execute(json!({
                "command": "sleep 5",
                "run_in_background": true,
                "timeout": 999
            }))
            .await
            .unwrap();

        let task_id = match result {
            ToolOutput::BackgroundTaskStarted { task_id, .. } => task_id,
            _ => panic!("Expected background task output"),
        };

        let task = registry.snapshot(&task_id).await.unwrap();
        assert_eq!(task.timeout_secs, MAX_TIMEOUT_SECS);
        let _ = registry.stop(&task_id).await;
    }

    #[tokio::test]
    async fn test_bash_timeout_exceeded() {
        let fixture = TestFixture::new();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let result = tool
            .execute(json!({
                "command": "sleep 10",
                "timeout": 1
            }))
            .await
            .unwrap();

        let output = rendered_command_output(result);

        assert_summary_value(&output, "timed_out", "true");
    }

    #[tokio::test]
    async fn test_bash_permission_deny() {
        let fixture = TestFixture::new();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let result = tool
            .execute(json!({
                "command": "rm -rf /"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Command not allowed"));
    }

    #[tokio::test]
    async fn env_prefixed_dangerous_command_is_denied_before_execution() {
        let fixture = TestFixture::new();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let err = tool
            .execute(json!({
                "command": "FOO=1 rm -rf /"
            }))
            .await
            .expect_err("env-prefixed dangerous command should be denied");

        assert!(err.to_string().contains("Command not allowed"));
    }

    #[tokio::test]
    async fn env_assignment_command_substitution_does_not_bypass_permission_prompt() {
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
        );
        let err = tool
            .execute(json!({
                "command": "X=$(touch${IFS}.pwned) cargo --version"
            }))
            .await
            .expect_err("unsafe env assignment should require permission for the original command");

        assert!(err.to_string().contains("noninteractive mode"));
        assert!(!fixture.project_root.join(".pwned").exists());
    }

    #[tokio::test]
    async fn allow_rule_cannot_lift_the_destructive_floor() {
        // `git branch *` is a built-in Allow rule, but `git branch -D` is a
        // destructive-tier shape; the floor must force a prompt (here surfaced
        // as a noninteractive error) rather than run it silently.
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
        );
        let err = tool
            .execute(json!({"command": "git branch -D some-branch"}))
            .await
            .expect_err("an allow-matched destructive command must still prompt");

        assert!(
            err.to_string().contains("noninteractive mode"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn allow_rule_does_not_waive_out_of_project_redirect() {
        // `echo *` / `git show *` are built-in Allow rules, but the out-of-tree
        // write-redirect riding on them is structural risk an allow rule must not
        // waive: it has to prompt (here, a noninteractive error) rather than run.
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
        );

        for command in [
            "echo probe > /tmp/bonsai_fix_a_probe",
            "git show HEAD:Cargo.toml >> /etc/hosts",
        ] {
            let err = tool
                .execute(json!({ "command": command }))
                .await
                .expect_err(&format!("`{command}` should prompt, not auto-run"));
            assert!(
                err.to_string().contains("noninteractive mode"),
                "`{command}` unexpected error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn destructive_command_with_redirect_prompts_at_auto_accept() {
        // Regression: a trailing out-of-tree redirect must not slide a destructive
        // command under the auto-accept ceiling. The redirect makes the segment
        // structural High — which auto-accept auto-approves — but the base command
        // is still a destructive shape, so the floor has to force a prompt (here a
        // noninteractive error). Covers both the Ask path (`git reset *`) and the
        // allow-matched path (`git branch -D` under the built-in `git branch *`).
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());
        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::with_level(crate::tool::ApprovalLevel::AutoAccept),
            CommandSandbox::disabled(),
        );

        for command in [
            "git reset --hard HEAD~5 > /tmp/bonsai_floor_probe",
            "git branch -D some-branch >> /tmp/bonsai_floor_probe",
        ] {
            let err = tool
                .execute(json!({ "command": command }))
                .await
                .expect_err(&format!("`{command}` must prompt, not auto-run"));
            assert!(
                err.to_string().contains("noninteractive mode"),
                "`{command}` unexpected error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn allowlisted_read_only_command_auto_approves_at_balanced() {
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());
        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::with_level(crate::tool::ApprovalLevel::Balanced),
            CommandSandbox::disabled(),
        );

        let output = tool
            .execute(json!({"command": "echo hi"}))
            .await
            .expect("a read-only command must auto-approve at Balanced");
        assert!(rendered_command_output(output).contains("hi"));
    }

    #[tokio::test]
    async fn wrapped_destructive_command_prompts_at_balanced() {
        // At the production-default Balanced level, medium-and-below auto-runs.
        // A wrapper-hidden `rm -rf` must resolve to its destructive tier and
        // prompt rather than ride through as a benign `env`/`time`/`xargs`.
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());
        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::with_level(crate::tool::ApprovalLevel::Balanced),
            CommandSandbox::disabled(),
        );

        for command in [
            "env rm -rf /tmp/x",
            "time rm -rf /tmp/x",
            "rm${IFS}-rf${IFS}/tmp/x",
        ] {
            let err = tool
                .execute(json!({ "command": command }))
                .await
                .expect_err(&format!("`{command}` should prompt at Balanced"));
            assert!(
                err.to_string().contains("noninteractive mode")
                    || err.to_string().contains("Command not allowed"),
                "`{command}` unexpected error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn out_of_tree_file_write_targets_prompt_at_balanced() {
        // `cp`/`ln`/`mkdir`/`touch`/`mv` are low-risk only when their write
        // targets stay project-local. Absolute/home targets must prompt at
        // Balanced instead of auto-running.
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());
        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
            Arc::new(BackgroundTaskRegistry::new()),
            YoloMode::with_level(crate::tool::ApprovalLevel::Balanced),
            CommandSandbox::disabled(),
        );

        let outside = fixture.temp_dir.path().join("outside-project");
        let outside_target = outside.join("bonsai-risk-probe");
        std::fs::create_dir_all(&outside).expect("outside temp directory should be created");
        std::fs::write(&outside_target, "probe\n").expect("outside target should be seeded");
        fixture.create_file("payload", "payload\n");
        let commands = [
            format!("mkdir {}", outside_target.display()),
            format!("touch {}", outside_target.display()),
            format!("cp payload {}", outside_target.display()),
            format!("ln -sf payload {}", outside_target.display()),
            format!("mv payload {}", outside_target.display()),
            format!("printf probe >| {}", outside_target.display()),
            format!("printf probe | tee {}", outside_target.display()),
            format!(
                "dd if=/dev/null of={} bs=1 count=0",
                outside_target.display()
            ),
            format!("sed -i.bak s/probe/fixed/ {}", outside_target.display()),
            format!("perl -pi -e 's/probe/fixed/' {}", outside_target.display()),
            format!("gawk -i inplace '{{ print }}' {}", outside_target.display()),
        ];

        for command in commands {
            let err = tool
                .execute(json!({ "command": command }))
                .await
                .expect_err(&format!("`{command}` should prompt at Balanced"));
            assert!(
                err.to_string().contains("noninteractive mode")
                    || err.to_string().contains("Command not allowed"),
                "`{command}` unexpected error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn compound_command_with_denied_segment_is_rejected() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let err = tool
            .execute(json!({"command": "echo ok && rm -rf /"}))
            .await
            .expect_err("denied segment in a compound command must be rejected");

        assert!(err.to_string().contains("Command not allowed"));
    }

    #[tokio::test]
    async fn large_output_is_truncated_to_a_unique_file() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let result = tool
            .execute(json!({"command": "seq 1 100000"}))
            .await
            .unwrap();

        match result {
            ToolOutput::Command {
                rendered,
                truncation,
                ..
            } => {
                assert!(rendered.contains("[Output truncated"));
                let ctx = truncation.expect("expected a truncation context");
                assert!(ctx.path.starts_with(".bonsai/tool-output/"));
                let canonical_root = fixture.project_root.canonicalize().unwrap();
                assert!(canonical_root.join(&ctx.path).exists());
            }
            _ => panic!("expected command output"),
        }
    }

    #[tokio::test]
    async fn non_yolo_cd_outside_root_resets_cwd_to_project_root() {
        let fixture = TestFixture::new();
        let outside = tempfile::TempDir::new().unwrap();
        let outside_path = outside.path().canonicalize().unwrap();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        tool.execute(json!({"command": format!("cd {}", outside_path.display())}))
            .await
            .unwrap();
        let pwd = rendered_command_output(tool.execute(json!({"command": "pwd"})).await.unwrap());

        assert_eq!(
            command_body(&pwd).trim(),
            fixture
                .project_root
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
    }

    #[tokio::test]
    async fn env_assignment_applies_to_one_bash_call_only() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let var = "__BONSAI_TEST_NONPERSISTENT_ENV";

        let first = rendered_command_output(
            tool.execute(json!({
                "command": format!("{var}=one printenv {var}")
            }))
            .await
            .unwrap(),
        );
        let second = rendered_command_output(
            tool.execute(json!({
                "command": format!("printenv {var}")
            }))
            .await
            .unwrap(),
        );

        assert_eq!(command_body(&first).trim(), "one");
        assert!(
            !command_body(&second).contains("one"),
            "env leaked into next call: {second}"
        );
    }

    #[tokio::test]
    async fn test_bash_permission_ask_blocks_without_responder() {
        let fixture = TestFixture::new();

        // Build a separate interaction service whose receiver is dropped so
        // the bash tool sees a cancelled prompt and returns an error.
        let (service, _drop) = crate::interaction::InteractionService::new();
        let service = Arc::new(service);
        drop(_drop);

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
        );
        let result = tool
            .execute(json!({
                "command": "true"
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn noninteractive_permission_ask_returns_clear_error() {
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
        );
        let err = tool
            .execute(json!({
                "command": "true"
            }))
            .await
            .expect_err("ask permission should fail in noninteractive mode");

        assert!(
            err.to_string().contains("noninteractive mode"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn yolo_bash_allows_ask_command_without_prompt() {
        let fixture = TestFixture::new();
        let service = Arc::new(crate::interaction::InteractionService::noninteractive());
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            service,
            Arc::new(BackgroundTaskRegistry::new()),
            yolo_mode,
            CommandSandbox::disabled(),
        );
        let output = rendered_command_output(
            tool.execute(json!({
                "command": "printf yolo-ask"
            }))
            .await
            .unwrap(),
        );

        assert_eq!(command_body(&output).trim(), "yolo-ask");
    }

    #[tokio::test]
    async fn yolo_bash_allows_denied_command_without_prompt() {
        let fixture = TestFixture::new();
        fixture
            .permissions
            .add_session_rule("printf *", Permission::Deny);
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            Arc::new(crate::interaction::InteractionService::noninteractive()),
            Arc::new(BackgroundTaskRegistry::new()),
            yolo_mode,
            CommandSandbox::disabled(),
        );
        let output = rendered_command_output(
            tool.execute(json!({
                "command": "printf yolo-deny"
            }))
            .await
            .unwrap(),
        );

        assert_eq!(command_body(&output).trim(), "yolo-deny");
    }

    #[tokio::test]
    async fn yolo_bash_allows_workdir_outside_project_root() {
        let fixture = TestFixture::new();
        let outside_dir = tempfile::TempDir::new().unwrap();
        let outside = outside_dir.path().canonicalize().unwrap();
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
            Arc::new(BackgroundTaskRegistry::new()),
            yolo_mode,
            CommandSandbox::disabled(),
        );
        let output = rendered_command_output(
            tool.execute(json!({
                "command": "pwd",
                "workdir": outside.display().to_string()
            }))
            .await
            .unwrap(),
        );

        assert_eq!(command_body(&output).trim(), outside.display().to_string());
    }

    #[tokio::test]
    async fn yolo_bash_persists_cd_outside_project_root_until_disabled() {
        let fixture = TestFixture::new();
        let outside_dir = tempfile::TempDir::new().unwrap();
        let outside = outside_dir.path().canonicalize().unwrap();
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = BashTool::with_background_tasks_and_yolo_mode(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
            Arc::new(BackgroundTaskRegistry::new()),
            yolo_mode.clone(),
            CommandSandbox::disabled(),
        );
        tool.execute(json!({
            "command": format!("cd {}", outside.display())
        }))
        .await
        .unwrap();
        let outside_pwd =
            rendered_command_output(tool.execute(json!({"command": "pwd"})).await.unwrap());

        yolo_mode.set_enabled(false);
        let project_pwd =
            rendered_command_output(tool.execute(json!({"command": "pwd"})).await.unwrap());

        assert_eq!(
            command_body(&outside_pwd).trim(),
            outside.display().to_string()
        );
        assert_eq!(
            command_body(&project_pwd).trim(),
            fixture
                .project_root
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
    }

    #[tokio::test]
    async fn test_bash_exit_code() {
        let fixture = TestFixture::new();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let result = tool
            .execute(json!({
                "command": "exit 42"
            }))
            .await
            .unwrap();

        let output = rendered_command_output(result);

        assert_summary_value(&output, "exit_code", "42");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_bash_workdir_with_symlinked_project_root() {
        let fixture = TestFixture::new();
        fixture.create_file("subdir/file.txt", "content");

        let alias_root = fixture.temp_dir.path().join("alias-root");
        std::os::unix::fs::symlink(&fixture.project_root, &alias_root).unwrap();

        let tool = BashTool::new(
            alias_root,
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let result = tool
            .execute(json!({
                "command": "pwd",
                "workdir": "subdir"
            }))
            .await
            .unwrap();

        let output = rendered_command_output(result);

        assert!(output.contains("subdir"), "output: {output}");
    }

    #[tokio::test]
    async fn test_bash_workdir_accepts_absolute_path_inside_project_root() {
        let fixture = TestFixture::new();
        fixture.create_file("subdir/file.txt", "content");

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let abs_workdir = fixture.project_root.join("subdir").canonicalize().unwrap();
        let result = tool
            .execute(json!({
                "command": "pwd",
                "workdir": abs_workdir.to_str().unwrap(),
            }))
            .await
            .unwrap();

        let output = rendered_command_output(result);
        assert!(output.contains("subdir"), "output: {output}");
    }

    #[tokio::test]
    async fn test_bash_workdir_rejects_absolute_path_outside_project_root() {
        let fixture = TestFixture::new();
        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        let err = tool
            .execute(json!({
                "command": "pwd",
                "workdir": "/tmp",
            }))
            .await
            .expect_err("absolute path outside project root must be rejected");
        assert!(
            err.to_string().contains("outside project root"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_bash_cat_marks_file_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "content");
        let canonical = file_path.canonicalize().unwrap();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        tool.execute(json!({"command": "cat test.txt"}))
            .await
            .unwrap();

        assert!(fixture.read_tracker.is_read(&canonical).await);
    }

    #[tokio::test]
    async fn bash_cat_read_allows_later_edit() {
        assert_bash_read_allows_edit("cat test.txt", "test.txt").await;
    }

    #[tokio::test]
    async fn bash_head_read_allows_later_edit() {
        assert_bash_read_allows_edit("head -n 1 test.txt", "test.txt").await;
    }

    #[tokio::test]
    async fn bash_tail_read_allows_later_edit() {
        assert_bash_read_allows_edit("tail -n 1 test.txt", "test.txt").await;
    }

    #[tokio::test]
    async fn bash_grep_read_allows_later_edit() {
        assert_bash_read_allows_edit("grep needle test.txt", "test.txt").await;
    }

    #[tokio::test]
    async fn bash_env_prefixed_grep_read_allows_later_edit() {
        assert_bash_read_allows_edit("FOO=1 grep needle test.txt", "test.txt").await;
    }

    #[tokio::test]
    async fn bash_recursive_grep_marks_matched_files_from_output() {
        assert_bash_read_allows_edit("grep -R needle dir", "dir/test.txt").await;
    }

    #[tokio::test]
    async fn bash_clustered_recursive_grep_marks_matched_files_from_output() {
        assert_bash_read_allows_edit("grep -RIn needle dir", "dir/test.txt").await;
    }

    #[tokio::test]
    async fn bash_attached_grep_regexp_read_allows_later_edit() {
        assert_bash_read_allows_edit("grep -eneedle test.txt", "test.txt").await;
    }

    #[tokio::test]
    async fn bash_grep_file_patterns_mark_pattern_and_target_files() {
        let fixture = TestFixture::new();
        let pattern_path = fixture.create_file("patterns.txt", "needle\n");
        let target_path = fixture.create_file("test.txt", "needle\nsecond\n");
        let pattern_canonical = pattern_path.canonicalize().unwrap();
        let target_canonical = target_path.canonicalize().unwrap();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        tool.execute(json!({"command": "grep -f patterns.txt test.txt"}))
            .await
            .unwrap();

        assert!(fixture.read_tracker.is_read(&pattern_canonical).await);
        assert!(fixture.read_tracker.is_read(&target_canonical).await);
    }

    #[tokio::test]
    async fn bash_read_marking_ignores_outside_root_paths() {
        let fixture = TestFixture::new();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "needle\n").unwrap();
        let outside_canonical = outside.path().canonicalize().unwrap();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        tool.execute(json!({"command": format!("cat {}", outside.path().display())}))
            .await
            .unwrap();

        assert!(!fixture.read_tracker.is_read(&outside_canonical).await);
    }

    #[tokio::test]
    async fn bash_read_marking_ignores_glob_operands() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "needle\nsecond\n");
        let canonical = file_path.canonicalize().unwrap();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        tool.execute(json!({"command": "cat *.txt"})).await.unwrap();

        assert!(!fixture.read_tracker.is_read(&canonical).await);
    }

    #[tokio::test]
    async fn bash_read_marking_ignores_pipeline_commands() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "needle\nsecond\n");
        let canonical = file_path.canonicalize().unwrap();

        let tool = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        tool.execute(json!({"command": "cat test.txt | grep needle"}))
            .await
            .unwrap();

        assert!(!fixture.read_tracker.is_read(&canonical).await);
    }

    async fn assert_bash_read_allows_edit(command: &str, path: &str) {
        let fixture = TestFixture::new();
        fixture.create_file(path, "needle\nsecond\n");

        let bash = BashTool::new(
            fixture.project_root.clone(),
            fixture.permissions.clone(),
            fixture.read_tracker.clone(),
            fixture.interaction.clone(),
        );
        bash.execute(json!({"command": command})).await.unwrap();

        let edit = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        edit.execute(json!({
            "path": path,
            "old_string": "needle",
            "new_string": "changed",
        }))
        .await
        .unwrap();
    }
}
