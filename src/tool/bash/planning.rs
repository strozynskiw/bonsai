//! Fail-closed command scope for the planning-mode Bash capability.
//!
//! Planning must not reuse the general autonomy classifier: many commands that
//! are routine in coding mode can write workspace state or execute project
//! configuration. This module accepts a deliberately small, parsed command
//! language and rebuilds it with a fixed trusted executable path before it is
//! handed to the shell sandbox.
//!
//! Local inspection tools resolve only from the root-owned system directories.
//! The collaboration clients (`gh`/`glab`) additionally resolve from fixed
//! package-manager binary directories; those candidates must stay inside the
//! matching installation prefix, be owned by root or the current user, have no
//! group/other-writable component in their directory chain, and never resolve
//! inside the project root. `PATH` is never consulted.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::command::tokenize_shell;

/// An accepted planning command and the effects its spawn requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanningCommandKind {
    /// A local, read-only inspection command.
    LocalRead,
    /// A GitHub or GitLab query. Its output is remote, untrusted data and its
    /// spawn needs network access.
    CollaborationRead,
    /// A GitHub or GitLab mutation such as closing or editing an issue.
    CollaborationWrite,
}

/// Parsed and canonicalized planning command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlanningCommand {
    command: String,
    kind: PlanningCommandKind,
}

impl PlanningCommand {
    pub(super) fn command(&self) -> &str {
        &self.command
    }

    pub(super) fn kind(&self) -> PlanningCommandKind {
        self.kind
    }

    pub(super) fn permits_network(&self) -> bool {
        !matches!(self.kind, PlanningCommandKind::LocalRead)
    }
}

/// Parse and canonically rebuild a command permitted in planning mode.
///
/// The accepted language deliberately excludes shell syntax, environment
/// prefixes, arbitrary executable paths, stdin, and option forms that could
/// open files or invoke editors. The returned string uses a fixed executable
/// path plus shell-escaped argument tokens, so the shell cannot resolve a
/// project-controlled program through `PATH`.
///
/// Rejects collaboration clients whose canonical binary resolves inside
/// `project_root` (a project-controlled executable must never run with network
/// access). Pass `None` when no project root is known.
pub(super) fn classify_planning_command_in(
    input: &str,
    project_root: Option<&Path>,
) -> Result<PlanningCommand> {
    let input = input.trim();
    if input.is_empty() {
        bail!("planning Bash command is required");
    }
    reject_shell_syntax(input)?;
    let tokens = tokenize_shell(input)
        .ok_or_else(|| anyhow::anyhow!("planning Bash command has unbalanced quotes"))?;
    let (program, args) = tokens
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("planning Bash command is required"))?;
    if program.contains('/') || program == "." || program == "source" || program.contains('=') {
        bail!("planning Bash only permits named trusted executables");
    }
    let executable = match program.as_str() {
        "gh" | "glab" => trusted_collaboration_executable(program, project_root),
        _ => trusted_executable(program),
    }
    .ok_or_else(|| anyhow::anyhow!("{program} is not an available planning-mode executable"))?;

    let kind = match program.as_str() {
        "pwd" if args.is_empty() => PlanningCommandKind::LocalRead,
        "ls" if args
            .iter()
            .all(|arg| is_safe_local_option(arg) || is_safe_workspace_path(arg)) =>
        {
            PlanningCommandKind::LocalRead
        }
        "cat" if !args.is_empty() && args.iter().all(|arg| is_safe_workspace_path(arg)) => {
            PlanningCommandKind::LocalRead
        }
        "head" | "tail" if valid_head_tail_args(args) => PlanningCommandKind::LocalRead,
        "grep" if valid_grep_args(args) => PlanningCommandKind::LocalRead,
        "git" if valid_git_query(args) => PlanningCommandKind::LocalRead,
        "gh" | "glab" => collaboration_kind(program, args)
            .ok_or_else(|| anyhow::anyhow!("command is outside the planning Bash allowlist"))?,
        _ => bail!("command is outside the planning Bash allowlist"),
    };
    if kind == PlanningCommandKind::LocalRead
        && !local_command_paths_stay_in_project(program, args, project_root)
    {
        bail!("planning Bash local paths must resolve inside the project root");
    }

    let execution_args = if program == "grep" {
        let (options, remaining) = split_grep_options(args)
            .ok_or_else(|| anyhow::anyhow!("command is outside the planning Bash allowlist"))?;
        options
            .iter()
            .cloned()
            .chain(std::iter::once("--".to_string()))
            .chain(remaining.iter().cloned())
            .collect::<Vec<_>>()
    } else {
        args.to_vec()
    };
    let mut command = if program == "git" {
        "GIT_OPTIONAL_LOCKS=0 ".to_string()
    } else {
        String::new()
    };
    command.push_str(&shell_quote(executable.to_string_lossy().as_ref()));
    if program == "git" {
        // Read-only Git commands can otherwise execute repository-configured
        // pagers, external diff drivers, or filesystem monitors. These
        // command-line settings override config without trusting it.
        for arg in [
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.pager=cat",
        ] {
            command.push(' ');
            command.push_str(&shell_quote(arg));
        }
    }
    for (index, arg) in execution_args.iter().enumerate() {
        command.push(' ');
        command.push_str(&shell_quote(arg));
        if program == "git" && index == 0 && matches!(arg.as_str(), "diff" | "show") {
            command.push_str(" '--no-ext-diff' '--no-textconv'");
        }
    }
    Ok(PlanningCommand { command, kind })
}

fn reject_shell_syntax(input: &str) -> Result<()> {
    if input.chars().any(|character| {
        matches!(
            character,
            '$' | '`' | '\\' | ';' | '|' | '&' | '<' | '>' | '\n' | '\r'
        )
    }) {
        bail!("planning Bash does not permit shell syntax, redirects, or substitutions");
    }
    Ok(())
}

/// Root-owned system binary directories trusted for every planning executable.
const SYSTEM_BIN_DIRS: &[&str] = &["/usr/bin", "/bin"];

/// Fixed package-manager binary directories, trusted only for the
/// collaboration clients. Entries are `prefix/bin` pairs; the canonical target
/// of a candidate must stay inside the matching prefix.
const COLLABORATION_BIN_DIRS: &[&str] = &[
    "/opt/homebrew/bin",              // Apple Silicon Homebrew
    "/usr/local/bin",                 // Intel Homebrew / admin installs
    "/opt/local/bin",                 // MacPorts
    "/home/linuxbrew/.linuxbrew/bin", // Linuxbrew
];

fn trusted_executable(program: &str) -> Option<PathBuf> {
    resolve_system_executable(program, SYSTEM_BIN_DIRS)
}

/// Resolve a collaboration client from the fixed system directories first,
/// then from the fixed package-manager directories.
fn trusted_collaboration_executable(program: &str, project_root: Option<&Path>) -> Option<PathBuf> {
    resolve_system_executable(program, SYSTEM_BIN_DIRS)
        .or_else(|| resolve_package_executable(program, COLLABORATION_BIN_DIRS, project_root))
}

fn resolve_system_executable(program: &str, dirs: &[&str]) -> Option<PathBuf> {
    dirs.iter()
        .filter_map(|directory| {
            let candidate = Path::new(directory).join(program);
            let canonical = candidate.canonicalize().ok()?;
            executable_file(&canonical).then_some(canonical)
        })
        .next()
}

/// Resolve `program` from a fixed package-manager `prefix/bin` directory.
///
/// The candidate must be a regular executable file owned by root or the
/// current user, have no group/other-writable bits, live inside `prefix`
/// (canonicalized, so symlinks into a Cellar are accepted but escapes are
/// not), have no writable component between `prefix` and the file, and never
/// resolve inside the project root.
fn resolve_package_executable<S: AsRef<str>>(
    program: &str,
    dirs: &[S],
    project_root: Option<&Path>,
) -> Option<PathBuf> {
    for directory in dirs {
        let directory = directory.as_ref();
        let prefix = Path::new(directory).parent()?;
        let Ok(prefix) = prefix.canonicalize() else {
            continue;
        };
        let candidate = Path::new(directory).join(program);
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if package_executable_file(&canonical, &prefix, project_root) {
            return Some(canonical);
        }
    }
    None
}

#[cfg(unix)]
fn package_executable_file(path: &Path, prefix: &Path, project_root: Option<&Path>) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(metadata) = path.metadata() else {
        return false;
    };
    let mode = metadata.permissions().mode();
    if !metadata.is_file() || mode & 0o111 == 0 || mode & 0o022 != 0 {
        return false;
    }
    let current_uid = nix::unistd::geteuid().as_raw();
    if metadata.uid() != 0 && metadata.uid() != current_uid {
        return false;
    }
    if let Some(root) = project_root {
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if path.starts_with(&canonical_root) {
            return false;
        }
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    protected_dir_chain(parent, prefix)
}

#[cfg(not(unix))]
fn package_executable_file(_path: &Path, _prefix: &Path, _project_root: Option<&Path>) -> bool {
    // The collaboration prefixes are unix paths; no candidate can exist on
    // other platforms, so fail closed rather than trusting any file.
    false
}

/// Require every directory from `start` up to and including `prefix` to be a
/// directory that is not group- or other-writable. Walking up to `prefix` also
/// enforces that the canonical target stays inside the prefix.
#[cfg(unix)]
fn protected_dir_chain(start: &Path, prefix: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let mut current = start;
    loop {
        let Ok(metadata) = current.metadata() else {
            return false;
        };
        if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
            return false;
        }
        if current == prefix {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        current = parent;
    }
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    path.metadata()
        .map(|metadata| {
            metadata.is_file()
                && metadata.uid() == 0
                && metadata.permissions().mode() & 0o133 == 0o111
        })
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn executable_file(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn is_safe_workspace_path(value: &str) -> bool {
    value != "-"
        && !value.is_empty()
        && !value.starts_with('-')
        && !value.starts_with('/')
        && !value
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
}

fn is_safe_local_option(value: &str) -> bool {
    value.starts_with('-')
        && value != "-"
        && !value.starts_with("--color=")
        && !matches!(value, "--help" | "--version")
}

fn local_command_paths_stay_in_project(
    program: &str,
    args: &[String],
    project_root: Option<&Path>,
) -> bool {
    let Some(project_root) = project_root else {
        return true;
    };
    let paths = match program {
        "pwd" | "git" => return true,
        "ls" => args
            .iter()
            .filter(|arg| !arg.starts_with('-'))
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "cat" => args.iter().map(String::as_str).collect(),
        "head" | "tail" => head_tail_paths(args),
        "grep" => split_grep_options(args)
            .and_then(|(_, remaining)| remaining.get(1..))
            .map(|paths| paths.iter().map(String::as_str).collect())
            .unwrap_or_default(),
        _ => return false,
    };
    let Ok(canonical_root) = project_root.canonicalize() else {
        return false;
    };
    paths.into_iter().all(|path| {
        canonical_root
            .join(path)
            .canonicalize()
            .is_ok_and(|resolved| resolved.starts_with(&canonical_root))
    })
}

fn head_tail_paths(args: &[String]) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut expects_count = false;
    for arg in args {
        if expects_count {
            expects_count = false;
        } else if matches!(arg.as_str(), "-n" | "-c" | "--lines" | "--bytes") {
            expects_count = true;
        } else if !arg.starts_with("--lines=") && !arg.starts_with("--bytes=") {
            paths.push(arg.as_str());
        }
    }
    paths
}

fn valid_head_tail_args(args: &[String]) -> bool {
    let mut expects_count = false;
    let mut saw_path = false;
    for arg in args {
        if expects_count {
            if arg.parse::<usize>().is_err() {
                return false;
            }
            expects_count = false;
        } else if matches!(arg.as_str(), "-n" | "-c" | "--lines" | "--bytes") {
            expects_count = true;
        } else if arg.starts_with("--lines=") || arg.starts_with("--bytes=") {
            if arg
                .split_once('=')
                .is_none_or(|(_, count)| count.parse::<usize>().is_err())
            {
                return false;
            }
        } else if is_safe_workspace_path(arg) {
            saw_path = true;
        } else {
            return false;
        }
    }
    !expects_count && saw_path
}

fn valid_grep_args(args: &[String]) -> bool {
    let Some((_options, remaining)) = split_grep_options(args) else {
        return false;
    };
    let Some((pattern, paths)) = remaining.split_first() else {
        return false;
    };
    !pattern.is_empty()
        && !pattern.starts_with('-')
        && !paths.is_empty()
        && paths.iter().all(|path| is_safe_workspace_path(path))
}

fn split_grep_options(args: &[String]) -> Option<(&[String], &[String])> {
    let pattern_index = args.iter().position(|arg| !arg.starts_with('-'))?;
    let (options, remaining) = args.split_at(pattern_index);
    if options.iter().any(|option| {
        !matches!(
            option.as_str(),
            "-i" | "-n" | "-H" | "-h" | "-s" | "-v" | "-w" | "-x" | "--fixed-strings"
        )
    }) {
        return None;
    }
    Some((options, remaining))
}

fn valid_git_query(args: &[String]) -> bool {
    let Some((subcommand, rest)) = args.split_first() else {
        return false;
    };
    let allowed_options: &[&str] = match subcommand.as_str() {
        "status" => &["--short", "--branch", "--porcelain", "--ignored"],
        "log" | "reflog" | "whatchanged" => &[
            "--oneline",
            "--decorate",
            "--all",
            "--graph",
            "--stat",
            "--name-only",
            "--name-status",
            "--reverse",
        ],
        "diff" | "show" => &[
            "--stat",
            "--name-only",
            "--name-status",
            "--patch",
            "--no-patch",
            "--color=never",
        ],
        "blame" => &[
            "--line-porcelain",
            "--porcelain",
            "--show-number",
            "--date=short",
        ],
        "shortlog" => &["--summary", "--numbered", "--email", "--all"],
        "describe" => &["--all", "--tags", "--always", "--long", "--dirty"],
        "ls-files" => &[
            "--cached",
            "--deleted",
            "--modified",
            "--others",
            "--exclude-standard",
            "--stage",
        ],
        "rev-parse" => &[
            "--verify",
            "--show-toplevel",
            "--is-inside-work-tree",
            "--is-inside-git-dir",
            "--git-dir",
        ],
        "grep" => &[
            "-n",
            "-i",
            "-w",
            "-v",
            "--name-only",
            "--full-name",
            "--cached",
        ],
        "cat-file" => &["-t", "-s", "-e", "-p"],
        "cherry" | "count-objects" | "name-rev" | "merge-base" | "branch" => &[],
        _ => return false,
    };
    rest.iter()
        .all(|arg| !arg.starts_with('-') || allowed_options.contains(&arg.as_str()))
}

fn collaboration_kind(program: &str, args: &[String]) -> Option<PlanningCommandKind> {
    if program == "gh" && args.len() == 1 && args.first().is_some_and(|arg| arg == "status") {
        return Some(PlanningCommandKind::CollaborationRead);
    }
    let (area, remaining) = args.split_first()?;
    let (action, arguments) = remaining.split_first()?;
    let area_allowed = match program {
        "gh" => matches!(area.as_str(), "issue" | "pr"),
        "glab" => matches!(area.as_str(), "issue" | "mr"),
        _ => false,
    };
    let grammar = collaboration_grammar(program, area, action)?;
    if !area_allowed || !valid_collaboration_arguments(arguments, grammar) {
        return None;
    }
    Some(if grammar.read_only {
        PlanningCommandKind::CollaborationRead
    } else {
        PlanningCommandKind::CollaborationWrite
    })
}

#[derive(Clone, Copy)]
struct CollaborationGrammar {
    read_only: bool,
    values: &'static [&'static str],
    flags: &'static [&'static str],
}

fn collaboration_grammar(program: &str, area: &str, action: &str) -> Option<CollaborationGrammar> {
    const LIST: CollaborationGrammar = CollaborationGrammar {
        read_only: true,
        values: &[
            "--state",
            "--author",
            "--assignee",
            "--label",
            "--search",
            "--limit",
        ],
        flags: &[],
    };
    const VIEW: CollaborationGrammar = CollaborationGrammar {
        read_only: true,
        values: &[],
        flags: &[],
    };
    const CREATE_ISSUE: CollaborationGrammar = CollaborationGrammar {
        read_only: false,
        values: &["--title", "--body", "--label", "--assignee", "--milestone"],
        flags: &[],
    };
    const CREATE_PR: CollaborationGrammar = CollaborationGrammar {
        read_only: false,
        values: &["--title", "--body", "--base"],
        flags: &["--draft"],
    };
    const EDIT_ISSUE: CollaborationGrammar = CollaborationGrammar {
        read_only: false,
        values: &[
            "--title",
            "--body",
            "--add-label",
            "--remove-label",
            "--assignee",
            "--milestone",
        ],
        flags: &[],
    };
    const EDIT_PR: CollaborationGrammar = CollaborationGrammar {
        read_only: false,
        values: &[
            "--title",
            "--body",
            "--base",
            "--add-label",
            "--remove-label",
            "--assignee",
            "--reviewer",
            "--milestone",
        ],
        flags: &[],
    };
    const CLOSE: CollaborationGrammar = CollaborationGrammar {
        read_only: false,
        values: &["--comment"],
        flags: &[],
    };
    const COMMENT: CollaborationGrammar = CollaborationGrammar {
        read_only: false,
        values: &["--body"],
        flags: &[],
    };
    const GLAB_CREATE: CollaborationGrammar = CollaborationGrammar {
        read_only: false,
        values: &[
            "--title",
            "--description",
            "--label",
            "--assignee",
            "--milestone",
        ],
        flags: &[],
    };
    const GLAB_UPDATE: CollaborationGrammar = CollaborationGrammar {
        read_only: false,
        values: &[
            "--title",
            "--description",
            "--label",
            "--assignee",
            "--milestone",
        ],
        flags: &[],
    };

    match (program, area, action) {
        ("gh", "issue" | "pr", "list") | ("glab", "issue" | "mr", "list") => Some(LIST),
        ("gh", "issue" | "pr", "view") | ("glab", "issue" | "mr", "view") => Some(VIEW),
        ("gh", "issue", "create") => Some(CREATE_ISSUE),
        ("gh", "pr", "create") => Some(CREATE_PR),
        ("gh", "issue", "edit") => Some(EDIT_ISSUE),
        ("gh", "pr", "edit") => Some(EDIT_PR),
        ("gh", "issue" | "pr", "close" | "reopen") => Some(CLOSE),
        ("gh", "issue" | "pr", "comment") => Some(COMMENT),
        ("glab", "issue" | "mr", "create") => Some(GLAB_CREATE),
        ("glab", "issue" | "mr", "update") => Some(GLAB_UPDATE),
        ("glab", "issue" | "mr", "close" | "reopen") => Some(VIEW),
        _ => None,
    }
}

fn valid_collaboration_arguments(args: &[String], grammar: CollaborationGrammar) -> bool {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "-"
            || argument.starts_with('/')
            || argument.starts_with('@')
            || argument.contains('\n')
        {
            return false;
        }
        if argument.starts_with("--") {
            let (option, inline_value) = argument.split_once('=').unwrap_or((argument, ""));
            if grammar.flags.contains(&option) {
                if !inline_value.is_empty() {
                    return false;
                }
            } else if grammar.values.contains(&option) {
                if inline_value.is_empty() {
                    let Some(value) = args.get(index + 1) else {
                        return false;
                    };
                    if value == "-" || value.starts_with('-') || value.starts_with('@') {
                        return false;
                    }
                    index += 1;
                }
            } else {
                return false;
            }
        } else if argument.starts_with('-') {
            return false;
        }
        index += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn present(program: &str) -> bool {
        trusted_executable(program)
            .or_else(|| trusted_collaboration_executable(program, None))
            .is_some()
    }

    #[cfg(unix)]
    fn temp_prefix(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bonsai-planning-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn package_dir(prefix: &Path) -> String {
        prefix.join("bin").to_str().unwrap().to_string()
    }

    #[test]
    fn rejects_shell_syntax_and_program_paths_before_spawn() {
        for command in [
            "./ls",
            "/tmp/ls",
            "ls > output",
            "cat $(touch owned)",
            "ls && touch owned",
            "cat < input",
            "env PATH=. ls",
            "sed -i src/main.rs",
            "sort -o output input",
            "xxd -r input output",
            "cargo check",
            "git diff --ext-diff",
            "git show --textconv",
            "git cat-file --filters HEAD:README.md",
            "git log --upload-pack=/tmp/payload",
            "git cat-file --batch-command",
            "grep --include=secret src",
            "gh issue list --jq '.[]'",
            "gh issue view 1 --body attacker",
            "glab mr list --description attacker",
        ] {
            assert!(
                classify_planning_command_in(command, None).is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn rebuilds_grep_with_an_end_of_options_marker() {
        let Some(command) = classify_planning_command_in("grep -n needle src/main.rs", None).ok()
        else {
            return;
        };
        assert!(command.command().contains("'-n' '--' 'needle'"));
    }

    #[test]
    fn accepts_exact_local_read_commands() {
        for command in [
            "pwd",
            "ls src",
            "cat src/main.rs",
            "head -n 5 src/main.rs",
            "grep Agent src/main.rs",
        ] {
            let result = classify_planning_command_in(command, None);
            if present(command.split_whitespace().next().unwrap()) {
                assert_eq!(
                    result.unwrap().kind(),
                    PlanningCommandKind::LocalRead,
                    "{command}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_read_paths_cannot_escape_through_symlinks() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("inside.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            project.path().join("outside.txt"),
        )
        .unwrap();

        assert!(classify_planning_command_in("cat inside.txt", Some(project.path())).is_ok());
        for command in [
            "cat outside.txt",
            "head -n 1 outside.txt",
            "grep secret outside.txt",
            "ls -L outside.txt",
        ] {
            assert!(
                classify_planning_command_in(command, Some(project.path())).is_err(),
                "{command}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonical_git_query_has_complete_config_arguments_and_executes() {
        let Some(status_command) = classify_planning_command_in("git status --short", None).ok()
        else {
            return;
        };
        let diff_command = classify_planning_command_in("git diff --stat", None).unwrap();
        let git = trusted_executable("git").unwrap();
        let root = tempfile::tempdir().unwrap();
        let initialized = std::process::Command::new(git)
            .args(["init", "-q"])
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(initialized.success());

        for command in [status_command, diff_command] {
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", command.command()])
                .current_dir(root.path())
                .output()
                .unwrap();

            assert!(
                output.status.success(),
                "{}: {}",
                command.command(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn accepts_collaboration_but_rejects_unsafe_forms_when_client_exists() {
        for command in [
            "gh issue close 42 --comment done",
            "gh pr list --state open",
            "glab issue update 42 --title fixed",
            "glab mr list --state opened",
        ] {
            let program = command.split_whitespace().next().unwrap();
            if present(program) {
                assert!(matches!(
                    classify_planning_command_in(command, None).unwrap().kind(),
                    PlanningCommandKind::CollaborationRead
                        | PlanningCommandKind::CollaborationWrite
                ));
            }
        }
        for command in [
            "gh api repos/acme/project/issues",
            "gh auth status",
            "glab api projects",
            "glab issue create --editor",
            "gh issue list --web",
            "gh issue create --body-file secret",
            "gh issue create --body @secret",
            "gh issue create --body -",
            "gh pr create --editor",
            "glab issue create --description-file secret",
        ] {
            assert!(
                classify_planning_command_in(command, None).is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn distinguishes_remote_reads_from_mutations() {
        let Some(_) = trusted_collaboration_executable("gh", None) else {
            return;
        };
        assert_eq!(
            classify_planning_command_in("gh issue list --state open", None)
                .unwrap()
                .kind(),
            PlanningCommandKind::CollaborationRead
        );
        assert_eq!(
            classify_planning_command_in("gh issue close 42 --comment done", None)
                .unwrap()
                .kind(),
            PlanningCommandKind::CollaborationWrite
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_executable_accepts_installed_binary() {
        let prefix = temp_prefix("accept");
        let bin = prefix.join("bin/gh");
        write_executable(&bin, 0o755);
        let dir = package_dir(&prefix);
        let dirs = [dir.as_str()];
        let resolved = resolve_package_executable("gh", &dirs, None);
        let expected = bin.canonicalize().unwrap();
        assert_eq!(resolved.as_deref(), Some(expected.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn package_executable_accepts_cellar_symlink() {
        let prefix = temp_prefix("cellar");
        let cellar_bin = prefix.join("Cellar/gh/2.0/bin");
        std::fs::create_dir_all(&cellar_bin).unwrap();
        let target = cellar_bin.join("gh");
        write_executable(&target, 0o755);
        std::os::unix::fs::symlink(&target, prefix.join("bin/gh")).unwrap();
        let dir = package_dir(&prefix);
        let dirs = [dir.as_str()];
        let resolved = resolve_package_executable("gh", &dirs, None);
        let expected = target.canonicalize().unwrap();
        assert_eq!(resolved.as_deref(), Some(expected.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn package_executable_rejects_escaping_symlink_and_project_binaries() {
        let prefix = temp_prefix("escape");
        let project = temp_prefix("project");
        let fake = project.join("bin/gh");
        write_executable(&fake, 0o755);
        std::os::unix::fs::symlink(&fake, prefix.join("bin/gh")).unwrap();
        let dir = package_dir(&prefix);
        let dirs = [dir.as_str()];
        assert!(resolve_package_executable("gh", &dirs, None).is_none());
        assert!(resolve_package_executable("gh", &dirs, Some(&project)).is_none());

        // A real (non-symlink) binary under the prefix passes every check
        // except the project-root guard, isolating that branch.
        let isolated = temp_prefix("isolated");
        write_executable(&isolated.join("bin/gh"), 0o755);
        let isolated_dir = package_dir(&isolated);
        let isolated_dirs = [isolated_dir.as_str()];
        assert!(resolve_package_executable("gh", &isolated_dirs, None).is_some());
        assert!(resolve_package_executable("gh", &isolated_dirs, Some(&isolated)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn package_executable_rejects_writable_binary_or_directory() {
        let writable_bin = temp_prefix("writable");
        let bin = writable_bin.join("bin/gh");
        write_executable(&bin, 0o777);
        let dir = package_dir(&writable_bin);
        let dirs = [dir.as_str()];
        assert!(resolve_package_executable("gh", &dirs, None).is_none());

        let writable_dir = temp_prefix("writable-dir");
        write_executable(&writable_dir.join("bin/gh"), 0o755);
        std::fs::set_permissions(
            writable_dir.join("bin"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let dir = package_dir(&writable_dir);
        let dirs = [dir.as_str()];
        assert!(resolve_package_executable("gh", &dirs, None).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn local_tools_never_resolve_from_package_directories() {
        let fake = temp_prefix("scope");
        write_executable(&fake.join("bin/ls"), 0o755);
        let command = classify_planning_command_in("ls src", None).unwrap();
        assert!(!command.command().contains(fake.to_str().unwrap()));
        assert!(command.command().contains("/usr/bin/ls") || command.command().contains("/bin/ls"));
    }
}
