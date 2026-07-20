use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

mod output;
pub(crate) use output::BashOutputBudget;

mod session;
use session::{EscapeApproval, SandboxEscapeGrants};

mod process;
use process::{CommandResult, CommandSummary, compact_summary_line};

mod policy;
use policy::emit_hook_warnings;

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
    pub(in crate::tool::bash) hooks: Arc<crate::hooks::HookEngine>,
    pub(in crate::tool::bash) authorization_ledger: AuthorizationLedger,
}

/// Runtime policy that must stay coherent for one Bash tool instance.
pub(crate) struct BashExecutionPolicy {
    yolo_mode: YoloMode,
    sandbox: CommandSandbox,
    hooks: Arc<crate::hooks::HookEngine>,
    authorization_ledger: AuthorizationLedger,
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
            hooks,
            authorization_ledger,
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
            hooks: self.hooks.clone(),
            authorization_ledger: self.authorization_ledger.clone(),
        }
    }
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
        "Execute a bash command from the project root/worktree by default. Do not prefix commands with `cd <repo> &&`; use `workdir` only when a different subdirectory is required. A leading `cd` back to the effective cwd is ignored. Supports timeout (seconds), persistent shell state (cd carries over), run_in_background:true for pipe-backed background execution, and interactive:true for a process-local PTY controlled with the terminal tool. Foreground output is returned with exit code and truncated to file if too large. Set `parallel: true` to run concurrently with other `parallel: true` bash calls in the same turn — the persistent shell `cwd` is then NOT updated, so do not use it on commands that `cd` or rely on shell state."
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "command",
                    string_property(
                        "The bash command to execute in the current shell cwd, initially the project root/worktree. Do not prefix commands with cd to the repo; a redundant leading cd is ignored.",
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
                    string_property(
                        "Working directory relative to the project root/worktree (optional; omit for the default cwd)",
                    ),
                ),
                (
                    "parallel",
                    boolean_property(
                        "Run concurrently with other parallel:true bash calls in the same turn. The persistent shell cwd is not updated when this is set.",
                    ),
                ),
                (
                    "run_in_background",
                    boolean_property(
                        "Start the command in the background and return immediately with a task ID. Background tasks default to a 900s timeout and do not update persistent cwd.",
                    ),
                ),
                (
                    "interactive",
                    boolean_property(
                        "Start the command in a real pseudo-terminal and return a pty-N ID immediately. Use the terminal tool to list, read, or stop it. Interactive mode cannot be combined with parallel, run_in_background, or escape_sandbox.",
                    ),
                ),
                (
                    "escape_sandbox",
                    boolean_property(
                        "Run this single command OUTSIDE the OS sandbox. Set true ONLY when the command legitimately must write outside the project root or use the network and the sandbox is blocking it (a confined command that failed will say so). Always requires explicit user approval; never set speculatively or to bypass permission rules. Reference out-of-root paths absolutely — escape does not relax the working-directory clamp. Not supported with run_in_background or interactive.",
                    ),
                ),
            ],
            &["command"],
        )
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
        let timeout_secs = effective_timeout_secs(args.timeout, run_in_background || interactive);
        let cwd = self.resolve_workdir(args.workdir.as_deref()).await?;
        let command = self
            .normalize_redundant_leading_cd(&args.command, &cwd)
            .await;
        let analysis = analyze_command(&command);

        let origin = context.as_ref().and_then(|ctx| ctx.origin());
        self.authorize_command(&command, &analysis, origin).await?;

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

        // escape_sandbox is a foreground-only argument: reject the combination
        // unconditionally (not only when the sandbox happens to be active), so the
        // contract holds in the default sandbox-off configuration too.
        if escape_sandbox && run_in_background {
            anyhow::bail!(
                "escape_sandbox is not supported for background tasks; run it in the foreground."
            );
        }

        // Sandbox-escape gate. Independent of and *after* the permission/risk gate
        // above: that decides whether the command may run (auto-approvable by the
        // autonomy level); this decides whether it runs outside the sandbox, and is
        // the enforcement floor — never auto-approved, even at yolo. A no-op unless
        // the model explicitly asked and the sandbox is actually active. Resolved
        // after `cwd` so the grant can be scoped to the directory the command runs
        // in (an approval doesn't leak to a different cwd at an unclamped level).
        let escape = if escape_sandbox && self.sandbox.is_active() {
            self.authorize_escape(&cwd, &command, origin).await?
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
            stdout,
            stderr,
            body,
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
                context.clone(),
            )
            .await?;

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

        let mut rendered = Self::finalize_foreground(
            &command, &body, exit_code, timed_out, confined, escape, &summary,
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
    fn finalize_foreground(
        command: &str,
        body: &str,
        exit_code: Option<i32>,
        timed_out: bool,
        confined: bool,
        escape: EscapeApproval,
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
        match escape {
            EscapeApproval::None => {
                // Soft nudge: a confined command failed and escape wasn't asked for.
                if confined && !timed_out && matches!(exit_code, Some(code) if code != 0) {
                    Self::push_paragraph(
                        &mut rendered,
                        "If this needs to run outside the sandbox (write outside the project root or use the network), retry with escape_sandbox=true.",
                    );
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
    fn finalize_foreground_nudges_escape_only_on_confined_failure() {
        let failed = summary_with_exit(Some(1));

        let nudged = BashTool::finalize_foreground(
            "false",
            "out",
            Some(1),
            false,
            true,
            EscapeApproval::None,
            &failed,
        );
        assert!(nudged.contains("escape_sandbox=true"), "{nudged}");
        assert!(nudged.contains("[Command summary]"), "{nudged}");

        // No nudge on success, outside confinement, or when the run already
        // escaped the sandbox.
        let succeeded = BashTool::finalize_foreground(
            "true",
            "out",
            Some(0),
            false,
            true,
            EscapeApproval::None,
            &summary_with_exit(Some(0)),
        );
        assert!(!succeeded.contains("escape_sandbox=true"), "{succeeded}");

        let unconfined = BashTool::finalize_foreground(
            "false",
            "out",
            Some(1),
            false,
            false,
            EscapeApproval::None,
            &failed,
        );
        assert!(!unconfined.contains("escape_sandbox=true"), "{unconfined}");

        let escaped = BashTool::finalize_foreground(
            "false",
            "out",
            Some(1),
            false,
            true,
            EscapeApproval::Once,
            &failed,
        );
        assert!(!escaped.contains("escape_sandbox=true"), "{escaped}");
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
        let sandbox = CommandSandbox::new(crate::sandbox::SandboxBackend::SeatbeltExec, root);
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
    async fn confined_failure_nudges_toward_escape() {
        // Real confinement (macOS) so the command is actually `confined`; a
        // non-zero exit without an escape request appends the retry nudge.
        let fixture = TestFixture::new();
        let service = Arc::new(InteractionService::noninteractive());
        let tool = escape_tool(&fixture, service, active_sandbox(&fixture.project_root));

        let output = tool
            .execute(json!({"command": "exit 3"}))
            .await
            .expect("a non-zero exit is not a tool error");
        assert!(rendered_command_output(output).contains("escape_sandbox=true"));
    }

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
