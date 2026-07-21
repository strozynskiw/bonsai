use super::{
    SandboxCommandRequest, SelfReviewCommandRequest, SerenityCommandRequest, SmolCommandRequest,
    parse_sandbox_command, parse_self_review_command, parse_serenity_command, parse_smol_command,
};
use crate::model_role::ModelShortcutKey;

pub(crate) struct CommandMetadata {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) usage_hint: Option<&'static str>,
    pub(crate) surface: CommandSurface,
}

/// What a slash command may do while the agent is mid-run. This is the single
/// declared source of truth for non-idle command behavior: the running
/// composer dispatcher (`tui::run::commands::handle_running_slash_command`)
/// and the busy-command modal (`tui::runtime_actions::submit_busy_command`)
/// both route through [`busy_behavior_for`] and share one pair of executors,
/// so the same input can never behave differently depending on which path
/// catches it. Idle-only capabilities (e.g. `/ctx`'s live preview) stay in the
/// idle dispatcher; any such asymmetry is documented on the arm that declares
/// it in [`busy_behavior_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusyCommandBehavior {
    /// Safe to serve immediately from cached/lock-free state (status views,
    /// cached reports/pickers). Must never contend for the agent lock, which
    /// the running turn holds until it finishes.
    ReadOnlyNow,
    /// A state change that genuinely needs the idle agent (e.g. switching the
    /// live model): offer to queue it; it runs when the current run ends.
    DeferUntilIdle,
    /// Applies fully mid-run through lock-free state (posture holders,
    /// storage, app mirrors). The user typing it mid-run means it.
    RunNow,
    /// Cannot run mid-run at all (would mutate the live conversation or start
    /// a second run); the modal offers cancel/dismiss.
    Block,
}

/// Where a command runs. `COMMANDS` is the single source of truth: the headless
/// dispatcher reads `surface` to decide whether to run a command or hand it off
/// with `headless_message`, so the command list and the dispatch stay in sync.
pub(crate) enum CommandSurface {
    /// Runs in both the TUI and headless (`bonsai -p`).
    Both,
    /// TUI-only. Headless callers get `headless_message`; when `strict_args` is
    /// set, a headless call with any argument is a usage error instead (these
    /// commands take no arguments).
    TuiOnly {
        headless_message: &'static str,
        strict_args: bool,
    },
}

const fn both() -> CommandSurface {
    CommandSurface::Both
}

const fn tui_only(headless_message: &'static str) -> CommandSurface {
    CommandSurface::TuiOnly {
        headless_message,
        strict_args: false,
    }
}

const fn tui_only_no_args(headless_message: &'static str) -> CommandSurface {
    CommandSurface::TuiOnly {
        headless_message,
        strict_args: true,
    }
}

/// Slash commands with the one-line description shown in `/help` and the
/// completion popover. Single source of truth — add new commands here.
pub(crate) const COMMANDS: &[CommandMetadata] = &[
    CommandMetadata {
        name: "/help",
        description: "List commands",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/keys",
        description: "Show keyboard shortcuts",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/ctx",
        description: "Visualize the current context window",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/episodes",
        description: "Show the episode ledger (task-scoped context lifecycle)",
        usage_hint: None,
        surface: tui_only_no_args(
            "/episodes is a TUI command — type /episodes inside the bonsai TUI to see episode lifecycle.",
        ),
    },
    CommandMetadata {
        name: "/perf",
        description: "Show performance and usage",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/cost",
        description: "Show performance and usage",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/usage",
        description: "Usage analytics across all projects",
        usage_hint: None,
        surface: tui_only_no_args("/usage is a TUI dashboard — run it inside the bonsai TUI."),
    },
    CommandMetadata {
        name: "/compact",
        description: "Compact context or preview the compaction plan",
        usage_hint: Some("preview target=<tokens> provider|deterministic"),
        surface: both(),
    },
    CommandMetadata {
        name: "/autonomy",
        description: "Set how much runs without asking",
        usage_hint: Some("ask|conservative|balanced|auto-accept|yolo|status"),
        surface: both(),
    },
    CommandMetadata {
        name: "/yolo",
        description: "Shortcut for /autonomy yolo (no guardrails)",
        usage_hint: Some("on|off|status"),
        surface: both(),
    },
    CommandMetadata {
        name: "/self-review",
        description: "Gate the self-review pass; `model` pins the reviewer model",
        usage_hint: Some("auto|on|ask|off|status|model"),
        surface: both(),
    },
    CommandMetadata {
        name: "/smol",
        description: "Configure the small-model compact profile",
        usage_hint: Some("on|off|status"),
        surface: both(),
    },
    CommandMetadata {
        name: "/serenity",
        description: "Use calm transcript presentation",
        usage_hint: Some("on|off|status"),
        surface: tui_only(
            "/serenity is a TUI presentation mode -- type /serenity inside the bonsai TUI to fold tool groups and hide thinking.",
        ),
    },
    CommandMetadata {
        name: "/permissions",
        description: "List or remove permission rules",
        usage_hint: Some("list|remove <id>"),
        surface: tui_only("Permission management is a TUI feature."),
    },
    CommandMetadata {
        name: "/sandbox",
        description: "Toggle OS-level command confinement",
        usage_hint: Some("on|off|net on|net off|status"),
        surface: both(),
    },
    CommandMetadata {
        name: "/config",
        description: "Show the layered .bonsai/config.toml view",
        usage_hint: Some("validate|edit [project|global]"),
        surface: both(),
    },
    CommandMetadata {
        name: "/bug",
        description: "Build a local, user-reviewed support bundle",
        usage_hint: Some("<description of the problem>"),
        surface: both(),
    },
    CommandMetadata {
        name: "/doctor",
        description: "Diagnose release-critical runtime dependencies",
        usage_hint: Some("[json] [online]"),
        surface: both(),
    },
    CommandMetadata {
        name: "/update",
        description: "Check for the newest release and install it in the background",
        usage_hint: None,
        surface: tui_only_no_args(
            "/update is a TUI command — run `bonsai update` from the CLI instead.",
        ),
    },
    CommandMetadata {
        name: "/mcp",
        description: "Inspect, control, and authorize MCP servers",
        usage_hint: Some("tools|enable|disable|reload|auth|callback|revoke <server>"),
        surface: both(),
    },
    CommandMetadata {
        name: "/hooks",
        description: "Show hook status; enable/disable/test",
        usage_hint: Some("enable|disable|test <name>"),
        surface: both(),
    },
    CommandMetadata {
        name: "/mode",
        description: "Switch runtime posture (autonomy, sandbox)",
        usage_hint: None,
        surface: tui_only(
            "/mode is a TUI picker — type /mode inside the bonsai TUI to switch runtime posture.",
        ),
    },
    CommandMetadata {
        name: "/settings",
        description: "Settings: model, autonomy, self-review, SMOL, sandbox, theme",
        usage_hint: None,
        surface: tui_only(
            "/settings is a TUI screen — type /settings inside the bonsai TUI to change settings.",
        ),
    },
    CommandMetadata {
        name: "/tasks",
        description: "List or stop background tasks",
        usage_hint: Some("stop <id>"),
        surface: both(),
    },
    CommandMetadata {
        name: "/init",
        description: "Generate a starter AGENTS.md for this project",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/skills",
        description: "List skills and built-in status; disable/enable built-ins",
        usage_hint: Some("[disable|enable <name>]"),
        surface: both(),
    },
    CommandMetadata {
        name: "/skill",
        description: "Load a skill's instructions into context",
        usage_hint: Some("<name>"),
        surface: both(),
    },
    CommandMetadata {
        name: "/agents",
        description: "Browse subagents; create/edit with the composer",
        usage_hint: Some("[new|init]"),
        surface: both(),
    },
    CommandMetadata {
        name: "/remember",
        description: "Save a fact to persistent memory",
        usage_hint: Some("[project] <fact>"),
        surface: both(),
    },
    CommandMetadata {
        name: "/memory",
        description: "Manage persistent memory entries (toggle, edit, delete)",
        usage_hint: Some("[<name>|forget <name>]"),
        surface: both(),
    },
    CommandMetadata {
        name: "/peers",
        description: "Show other bonsai sessions in this project (live)",
        usage_hint: None,
        surface: tui_only_no_args(
            "/peers is a TUI view — type /peers inside the bonsai TUI to see the other bonsai sessions in this project.",
        ),
    },
    CommandMetadata {
        name: "/subagents",
        description: "Show running subagents (live)",
        usage_hint: None,
        surface: tui_only_no_args(
            "/subagents is a TUI view — type /subagents (or press Alt+S) inside the bonsai TUI to watch running subagents.",
        ),
    },
    CommandMetadata {
        name: "/start",
        description: "Implement the current plan",
        usage_hint: None,
        surface: tui_only_no_args(
            "/start is a TUI plan-handoff command — open the plan view, draft a plan, then type /start.",
        ),
    },
    CommandMetadata {
        name: "/continue",
        description: "Resume plan implementation from the next pending phase",
        usage_hint: None,
        surface: tui_only_no_args(
            "/continue resumes phased plan execution from where it left off.",
        ),
    },
    CommandMetadata {
        name: "/test",
        description: "Run the configured test verification profile",
        usage_hint: None,
        surface: tui_only_no_args(
            "/test currently runs as a focused TUI verification workflow; use it inside the bonsai TUI.",
        ),
    },
    CommandMetadata {
        name: "/build",
        description: "Run the configured build verification profile",
        usage_hint: None,
        surface: tui_only_no_args(
            "/build currently runs as a focused TUI verification workflow; use it inside the bonsai TUI.",
        ),
    },
    CommandMetadata {
        name: "/retry",
        description: "Retry the latest user turn",
        usage_hint: Some("<model-shortcut>"),
        surface: tui_only(
            "/retry currently continues an interactive TUI turn; use it inside the bonsai TUI.",
        ),
    },
    CommandMetadata {
        name: "/save",
        description: "Save the current plan",
        usage_hint: None,
        surface: tui_only("Plan commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/discard",
        description: "Discard the current plan (deletes it if saved)",
        usage_hint: None,
        surface: tui_only("Plan commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/export",
        description: "Export the current plan to a file",
        usage_hint: Some("<path>"),
        surface: tui_only("Plan commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/plans",
        description: "Open saved plans",
        usage_hint: None,
        surface: tui_only("Plan commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/sessions",
        description: "List saved sessions",
        usage_hint: None,
        surface: tui_only("Session persistence commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/resume",
        description: "Resume a saved session",
        usage_hint: Some("[id]"),
        surface: tui_only("Session persistence commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/forget",
        description: "Delete a saved session",
        usage_hint: Some("<id>"),
        surface: tui_only("Session persistence commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/search",
        description: "Search saved messages",
        usage_hint: Some("<query>"),
        surface: tui_only("Session persistence commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/authorize",
        description: "Choose a provider to authorize",
        usage_hint: Some("[provider]"),
        surface: both(),
    },
    CommandMetadata {
        name: "/unauthorize",
        description: "Remove a provider authorization",
        usage_hint: Some("provider"),
        surface: both(),
    },
    CommandMetadata {
        name: "/model",
        description: "Switch model",
        usage_hint: Some("<number|model|connection:model>"),
        surface: both(),
    },
    CommandMetadata {
        name: "/refresh",
        description: "Refresh model catalogs and provider lists",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/providers",
        description: "Manage providers: add, edit, remove, disable",
        usage_hint: Some("[list|add|edit <id>|remove <id>|disable <id>|enable <id>]"),
        surface: both(),
    },
    CommandMetadata {
        name: "/wizard",
        description: "Alias for /providers add (create or edit a custom provider)",
        usage_hint: Some("<provider-id>"),
        surface: tui_only(
            "/wizard is a TUI command — type /wizard inside the bonsai TUI to create or edit a custom provider.",
        ),
    },
    CommandMetadata {
        name: "/new",
        description: "Start a new session",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/new-plan",
        description: "Reset the plan canvas (keep chat & context)",
        usage_hint: None,
        surface: tui_only("Plan commands are a TUI feature."),
    },
    CommandMetadata {
        name: "/clear",
        description: "Clear the conversation",
        usage_hint: None,
        surface: both(),
    },
    CommandMetadata {
        name: "/copy",
        description: "Copy selection or focused row",
        usage_hint: Some("all"),
        surface: tui_only(
            "/copy is handled by the TUI: selected text or focused row; /copy all copies the full transcript.",
        ),
    },
    CommandMetadata {
        name: "/commit",
        description: "Commit pending changes",
        usage_hint: None,
        surface: tui_only_no_args(
            "/commit is a TUI command — type /commit inside the bonsai TUI to create a git commit.",
        ),
    },
    CommandMetadata {
        name: "/pr",
        description: "Create or update a pull request",
        usage_hint: None,
        surface: tui_only_no_args(
            "/pr is a TUI command — type /pr inside the bonsai TUI to create or update a pull request.",
        ),
    },
    CommandMetadata {
        name: "/review",
        description: "Review pending changes",
        usage_hint: None,
        surface: tui_only_no_args(
            "/review is a TUI command — type /review inside the bonsai TUI to review pending changes.",
        ),
    },
    CommandMetadata {
        name: "/security-review",
        description: "Security-review uncommitted changes (read-only)",
        usage_hint: None,
        surface: tui_only_no_args(
            "/security-review is a TUI command — type /security-review inside the bonsai TUI to audit uncommitted changes.",
        ),
    },
    CommandMetadata {
        name: "/theme",
        description: "Switch color theme (or export <name>)",
        usage_hint: Some("theme"),
        surface: tui_only("Theme switching is a TUI feature — use /theme inside the bonsai TUI."),
    },
    CommandMetadata {
        name: "/bonsai",
        description: "Grow a fresh bonsai tree",
        usage_hint: None,
        surface: tui_only_no_args(
            "/bonsai is a TUI decoration — run it inside the bonsai TUI to regrow the tree.",
        ),
    },
    CommandMetadata {
        name: "/quit",
        description: "Exit bonsai",
        usage_hint: None,
        surface: both(),
    },
];

pub(crate) fn command_description(name: &str) -> Option<&'static str> {
    command_metadata(name).map(|command| command.description)
}

pub(crate) fn command_surface(name: &str) -> Option<&'static CommandSurface> {
    command_metadata(name).map(|command| &command.surface)
}

pub(crate) fn command_usage_hint(name: &str) -> Option<&'static str> {
    command_metadata(name).and_then(|command| command.usage_hint)
}

pub(crate) fn command_completion_preview(name: &str) -> Option<String> {
    let description = command_description(name)?;
    match command_usage_hint(name) {
        Some(hint) => Some(format!("{description} [{hint}]")),
        None => Some(description.to_string()),
    }
}

/// Map a parsed setting-toggle command to its busy behavior: a status/read
/// query is safe to run now, a valid set applies immediately (these toggles
/// mutate lock-free posture holders / storage, never the running turn), and an
/// unparseable invocation is blocked. Reusing the command's own parser keeps
/// the arg grammar in a single place rather than re-encoding (and drifting
/// from) it.
fn set_or_status_busy<T>(
    parsed: Result<T, String>,
    is_status: impl FnOnce(&T) -> bool,
) -> BusyCommandBehavior {
    match parsed {
        Ok(request) if is_status(&request) => BusyCommandBehavior::ReadOnlyNow,
        Ok(_) => BusyCommandBehavior::RunNow,
        Err(_) => BusyCommandBehavior::Block,
    }
}

pub(crate) fn busy_behavior_for(input: &str) -> BusyCommandBehavior {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return BusyCommandBehavior::Block;
    }
    let mut parts = trimmed.split_whitespace();
    let Some(command) = parts.next() else {
        return BusyCommandBehavior::Block;
    };
    let args = parts.collect::<Vec<_>>();

    match canonical_command_name(command) {
        "/help" | "/keys" | "/ctx" | "/episodes" | "/perf" | "/cost" | "/usage" | "/sessions"
        | "/skills" | "/agents" | "/subagents" | "/peers" => BusyCommandBehavior::ReadOnlyNow,
        "/tasks" if args.is_empty() => BusyCommandBehavior::ReadOnlyNow,
        // Setting toggles apply mid-run: their sets go through lock-free state
        // (sandbox handle, storage, app mirrors) so `/sandbox off` typed while
        // the agent is busy takes effect immediately instead of bouncing into a
        // status view or a queue modal. Delegate to each command's real parser
        // so the arg grammar lives in one place instead of being re-encoded
        // (and drifting) here — the old hand-written `/self-review` arm listed
        // only `auto|default|on|ask|off` and wrongly blocked the equally-valid
        // `no|yes|true|false` mid-run.
        "/sandbox" => set_or_status_busy(parse_sandbox_command(trimmed), |request| {
            matches!(request, SandboxCommandRequest::Status)
        }),
        "/self-review" => set_or_status_busy(parse_self_review_command(trimmed), |request| {
            matches!(request, SelfReviewCommandRequest::Status)
        }),
        "/smol" => set_or_status_busy(parse_smol_command(trimmed), |request| {
            matches!(request, SmolCommandRequest::Status)
        }),
        "/serenity" => set_or_status_busy(parse_serenity_command(trimmed), |request| {
            matches!(request, SerenityCommandRequest::Status)
        }),
        "/model" if args.is_empty() => BusyCommandBehavior::ReadOnlyNow,
        "/model" => BusyCommandBehavior::DeferUntilIdle,
        // Listing providers is read-only; mutations (add/edit/remove/…) touch
        // catalog files and the live provider, so they block mid-run.
        "/providers" if matches!(args.as_slice(), [] | ["list"]) => {
            BusyCommandBehavior::ReadOnlyNow
        }
        "/providers" => BusyCommandBehavior::Block,
        command if args.is_empty() && ModelShortcutKey::from_command(command).is_some() => {
            BusyCommandBehavior::DeferUntilIdle
        }
        // Memory writes go to the file store, not the live conversation, so
        // they are safe mid-run; list/view only read the store snapshot.
        "/remember" => BusyCommandBehavior::RunNow,
        "/memory" => match args.as_slice() {
            ["forget", _] => BusyCommandBehavior::RunNow,
            [] | [_] => BusyCommandBehavior::ReadOnlyNow,
            _ => BusyCommandBehavior::Block,
        },
        "/theme" | "/save" | "/export" | "/plans" | "/search" | "/permissions" | "/autonomy"
        | "/yolo" | "/mode" | "/copy" => BusyCommandBehavior::RunNow,
        // A bug bundle gathered mid-run would describe a half-finished run and
        // needs the agent lock the run holds; deferring gets the completion
        // report of the run the user is probably reporting about.
        "/bug" => BusyCommandBehavior::DeferUntilIdle,
        // `/settings` seeds its SMOL row from the live agent profile, which
        // needs the agent lock a running turn holds — so unlike `/mode` (fully
        // seeded from lock-free app state) it cannot open mid-run.
        "/settings" => BusyCommandBehavior::Block,
        "/commit" | "/pr" | "/security-review" | "/retry" | "/compact" | "/clear" | "/new"
        | "/resume" | "/forget" | "/unauthorize" | "/refresh" | "/wizard" | "/start"
        | "/continue" | "/init" | "/skill" | "/quit" => BusyCommandBehavior::Block,
        // `/authorize` and `/review` show informational pickers that are safe
        // to browse mid-run; the actual action (auth / review task) only starts
        // when the user confirms inside the picker.
        "/authorize" | "/review" => BusyCommandBehavior::RunNow,
        // `/update` runs in its own detached background task guarded by the
        // update file lock and never touches the agent or the conversation.
        // Like `/providers`, it is caught by the any-task-state block in
        // event_loop.rs before either non-idle executor sees it.
        "/update" => BusyCommandBehavior::RunNow,
        _ => BusyCommandBehavior::Block,
    }
}

/// Map legacy command spellings to their canonical name, so renames don't
/// break muscle memory. Completion and /help advertise only canonical names.
pub(crate) fn canonical_command_name(name: &str) -> &str {
    if name.eq_ignore_ascii_case("/subtasks") {
        "/subagents"
    } else {
        name
    }
}

fn command_metadata(name: &str) -> Option<&'static CommandMetadata> {
    let name = canonical_command_name(name);
    COMMANDS
        .iter()
        .find(|command| command.name.eq_ignore_ascii_case(name))
}

pub fn complete_command(prefix: &str) -> Option<String> {
    if !prefix.starts_with('/') {
        return None;
    }
    let lower_prefix = prefix.to_lowercase();
    let matches: Vec<String> = COMMANDS
        .iter()
        .map(|command| command.name)
        .filter(|name| name.to_lowercase().starts_with(&lower_prefix))
        .map(str::to_string)
        .collect();
    if matches.is_empty() {
        return None;
    }
    if let [single] = matches.as_slice() {
        return Some(single.clone());
    }
    let mut matches = matches;
    matches.sort();
    let common = longest_common_prefix_owned(&matches);
    // Extend to the common prefix when it is longer than what the user typed, or
    // normalize the case of an exact-but-differently-cased prefix (e.g. `/C` ->
    // `/c`); otherwise echo the prefix back so the popover stays open.
    if common.len() > prefix.len() || common != prefix {
        Some(common)
    } else {
        Some(prefix.to_string())
    }
}

/// Given candidate strings already filtered to those matching `arg`, return the
/// completion to apply: the sole match, or the longest common prefix when it
/// extends past `arg`. `None` when there is nothing to add (no matches, or the
/// common prefix is no longer than what the user already typed).
pub(crate) fn complete_from_matches(mut matches: Vec<String>, arg: &str) -> Option<String> {
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [] => None,
        [single] => Some(single.clone()),
        _ => {
            let common = longest_common_prefix_owned(&matches);
            (common.len() > arg.len()).then_some(common)
        }
    }
}

fn longest_common_prefix_owned(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let first = &strings[0];
    let mut len = first.chars().count();
    for s in &strings[1..] {
        let s_chars = s.chars().count();
        len = len.min(s_chars);
        for (i, (a, b)) in first.chars().zip(s.chars()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }
    first.chars().take(len).collect()
}
