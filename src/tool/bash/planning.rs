//! Fail-closed command scope for the planning-mode Bash capability.
//!
//! Planning must not reuse the general autonomy classifier: many commands that
//! are routine in coding mode can write workspace state or execute project
//! configuration. This module accepts a deliberately small, parsed command
//! language and rebuilds it with a fixed trusted executable path before it is
//! handed to the shell sandbox.

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
pub(super) fn classify_planning_command(input: &str) -> Result<PlanningCommand> {
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
    let executable = trusted_executable(program)
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
            "-c",
        ] {
            command.push(' ');
            command.push_str(&shell_quote(arg));
        }
    }
    if program == "git" && matches!(args.first().map(String::as_str), Some("diff" | "show")) {
        command.push_str(" '--no-ext-diff'");
    }
    for arg in &execution_args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
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

fn trusted_executable(program: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin"]
        .into_iter()
        .filter_map(|directory| Path::new(directory).join(program).canonicalize().ok())
        .find(|path| executable_file(path))
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

    fn present(program: &str) -> bool {
        trusted_executable(program).is_some()
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
            assert!(classify_planning_command(command).is_err(), "{command}");
        }
    }

    #[test]
    fn rebuilds_grep_with_an_end_of_options_marker() {
        let Some(command) = classify_planning_command("grep -n needle src/main.rs").ok() else {
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
            let result = classify_planning_command(command);
            if present(command.split_whitespace().next().unwrap()) {
                assert_eq!(
                    result.unwrap().kind(),
                    PlanningCommandKind::LocalRead,
                    "{command}"
                );
            }
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
                    classify_planning_command(command).unwrap().kind(),
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
            assert!(classify_planning_command(command).is_err(), "{command}");
        }
    }

    #[test]
    fn distinguishes_remote_reads_from_mutations() {
        let Some(_) = trusted_executable("gh") else {
            return;
        };
        assert_eq!(
            classify_planning_command("gh issue list --state open")
                .unwrap()
                .kind(),
            PlanningCommandKind::CollaborationRead
        );
        assert_eq!(
            classify_planning_command("gh issue close 42 --comment done")
                .unwrap()
                .kind(),
            PlanningCommandKind::CollaborationWrite
        );
    }
}
