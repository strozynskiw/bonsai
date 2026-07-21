//! Static shell-completion scripts for the supported command-line surface.

/// Shells with first-party completion output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

impl CompletionShell {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }
}

/// Render a completion script without consulting user configuration or the network.
pub(crate) fn render(shell: CompletionShell) -> &'static str {
    match shell {
        CompletionShell::Bash => BASH,
        CompletionShell::Zsh => ZSH,
        CompletionShell::Fish => FISH,
    }
}

const BASH: &str = r#"_bonsai() {
  local cur prev command
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]:-}"
  command="${COMP_WORDS[1]:-}"

  case "$prev" in
    --output-format) COMPREPLY=( $(compgen -W "text json stream-json" -- "$cur") ); return ;;
    --autonomy) COMPREPLY=( $(compgen -W "ask conservative balanced auto-accept yolo" -- "$cur") ); return ;;
    --isolation) COMPREPLY=( $(compgen -W "auto worktree off" -- "$cur") ); return ;;
    --mode) COMPREPLY=( $(compgen -W "mock live" -- "$cur") ); return ;;
    --effort) COMPREPLY=( $(compgen -W "none low medium high xhigh" -- "$cur") ); return ;;
    completions) COMPREPLY=( $(compgen -W "bash zsh fish" -- "$cur") ); return ;;
  esac

  if (( COMP_CWORD == 1 )); then
    COMPREPLY=( $(compgen -W "eval model-catalog doctor update recovery completions -h --help -V --version -c --continue -p --print --isolation --reduced-motion --screen-reader" -- "$cur") )
    return
  fi

  case "$command" in
    doctor)
      COMPREPLY=( $(compgen -W "--json --online -h --help" -- "$cur") )
      return
      ;;
    update)
      COMPREPLY=( $(compgen -W "--check -h --help" -- "$cur") )
      return
      ;;
    recovery)
      COMPREPLY=( $(compgen -W "list inspect merge keep discard" -- "$cur") )
      return
      ;;
    model-catalog)
      COMPREPLY=( $(compgen -W "check" -- "$cur") )
      return
      ;;
    completions)
      COMPREPLY=( $(compgen -W "bash zsh fish" -- "$cur") )
      return
      ;;
    eval)
      COMPREPLY=( $(compgen -W "adapter --suite --mode --provider --model --effort --task --seed --out --baseline --json --fail-on-task-failure -h --help" -- "$cur") )
      return
      ;;
  esac

  if [[ "$cur" == -* ]]; then
    COMPREPLY=( $(compgen -W "-h --help -V --version -c --continue -p --print --output-format --autonomy --yolo --model --effort --max-turns --max-generation-seconds --max-output-chars --max-tool-seconds --append-system-prompt --timeout --isolation --reduced-motion --screen-reader" -- "$cur") )
  fi
}
complete -F _bonsai bonsai
"#;

const ZSH: &str = r#"#compdef bonsai

_bonsai() {
  local previous="${words[CURRENT-1]}"
  case "$previous" in
    --output-format) _values 'output format' text json stream-json; return ;;
    --autonomy) _values 'autonomy' ask conservative balanced auto-accept yolo; return ;;
    --isolation) _values 'isolation' auto worktree off; return ;;
    --mode) _values 'eval mode' mock live; return ;;
    --effort) _values 'reasoning effort' none low medium high xhigh; return ;;
    completions) _values 'shell' bash zsh fish; return ;;
  esac

  if (( CURRENT == 2 )); then
    local -a commands
    commands=(
      'eval:run an evaluation suite or adapter'
      'model-catalog:validate a models.dev catalog'
      'doctor:run release diagnostics'
      'update:install the newest signed release'
      'recovery:manage recovery worktrees'
      'completions:generate a shell completion script'
      '-h:show help'
      '--help:show help'
      '-V:show version'
      '--version:show version'
    )
    _describe 'bonsai command' commands
    return
  fi

  case "${words[2]}" in
    doctor) _values 'doctor option' --json --online -h --help; return ;;
    update) _values 'update option' --check -h --help; return ;;
    recovery) _values 'recovery command' list inspect merge keep discard; return ;;
    model-catalog) _values 'model catalog command' check; return ;;
    completions) _values 'shell' bash zsh fish; return ;;
    eval) _values 'eval option' adapter --suite --mode --provider --model --effort --task --seed --out --baseline --json --fail-on-task-failure; return ;;
  esac

  if [[ "$PREFIX" == -* ]]; then
    _values 'bonsai option' -h --help -V --version -c --continue -p --print --output-format --autonomy --yolo --model --effort --max-turns --max-generation-seconds --max-output-chars --max-tool-seconds --append-system-prompt --timeout --isolation --reduced-motion --screen-reader
  fi
}

compdef _bonsai bonsai
"#;

const FISH: &str = r#"function __fish_bonsai_needs_command
    test (count (commandline -opc)) -eq 1
end

complete -c bonsai -f
complete -c bonsai -n __fish_bonsai_needs_command -a eval -d 'Run an evaluation suite or adapter'
complete -c bonsai -n __fish_bonsai_needs_command -a model-catalog -d 'Validate a models.dev catalog'
complete -c bonsai -n __fish_bonsai_needs_command -a doctor -d 'Run release diagnostics'
complete -c bonsai -n __fish_bonsai_needs_command -a update -d 'Install the newest signed release'
complete -c bonsai -n __fish_bonsai_needs_command -a recovery -d 'Manage recovery worktrees'
complete -c bonsai -n __fish_bonsai_needs_command -a completions -d 'Generate a shell completion script'
complete -c bonsai -s h -l help -d 'Show help'
complete -c bonsai -s V -l version -d 'Show version'
complete -c bonsai -s c -l continue -d 'Resume the latest or selected session'
complete -c bonsai -s p -l print -d 'Run one headless agent turn'
complete -c bonsai -l output-format -xa 'text json stream-json'
complete -c bonsai -l autonomy -xa 'ask conservative balanced auto-accept yolo'
complete -c bonsai -l isolation -xa 'auto worktree off'
complete -c bonsai -l reduced-motion -d 'Disable TUI animation'
complete -c bonsai -l screen-reader -d 'Use linear accessible TUI output'
complete -c bonsai -l effort -xa 'none low medium high xhigh'
complete -c bonsai -n '__fish_seen_subcommand_from doctor' -l json
complete -c bonsai -n '__fish_seen_subcommand_from doctor' -l online
complete -c bonsai -n '__fish_seen_subcommand_from update' -l check
complete -c bonsai -n '__fish_seen_subcommand_from recovery' -a 'list inspect merge keep discard'
complete -c bonsai -n '__fish_seen_subcommand_from model-catalog' -a check
complete -c bonsai -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_covers_the_stable_top_level_surface() {
        for shell in [
            CompletionShell::Bash,
            CompletionShell::Zsh,
            CompletionShell::Fish,
        ] {
            let script = render(shell);
            for token in [
                "eval",
                "model-catalog",
                "doctor",
                "update",
                "recovery",
                "completions",
            ] {
                assert!(script.contains(token), "{shell:?} omits {token}");
            }
            for option in [
                "output-format",
                "autonomy",
                "isolation",
                "reduced-motion",
                "screen-reader",
            ] {
                assert!(script.contains(option), "{shell:?} omits --{option}");
            }
            assert!(script.ends_with('\n'));
        }
    }

    #[test]
    fn shell_names_parse_strictly() {
        assert_eq!(CompletionShell::parse("bash"), Some(CompletionShell::Bash));
        assert_eq!(CompletionShell::parse("zsh"), Some(CompletionShell::Zsh));
        assert_eq!(CompletionShell::parse("fish"), Some(CompletionShell::Fish));
        assert_eq!(CompletionShell::parse("Bash"), None);
    }
}
