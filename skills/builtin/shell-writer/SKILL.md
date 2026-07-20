---
name: shell-writer
description: Shell scripting correctness and structure — dialect detection, strict-mode headers, quoting discipline, clean script layout, and the silent failure modes of pipes, subshells, and word splitting. Load before writing or editing shell scripts.
activation:
  extensions: [sh, bash, zsh]
---

# Shell Writer

How to write shell that fails loudly instead of succeeding wrongly: know which
dialect the file is, quote every expansion, and assume any command can fail.

## Dialect first

- The shebang decides the language. `#!/bin/sh` means POSIX only: no arrays,
  no `[[ ]]`, no `local -n`, no `${var,,}`. `#!/usr/bin/env bash` unlocks
  bashisms. Match the file's existing shebang — don't add bash features to an
  `sh` script.
- New bash scripts start with strict mode:

  ```bash
  #!/usr/bin/env bash
  set -euo pipefail
  ```

  Know its limits: `-e` is suppressed inside `if`/`while` conditions and on
  the left of `||`/`&&`, and `local x=$(cmd)` masks `cmd`'s exit code — declare
  and assign on separate lines when the status matters.

## Quote everything

- `"$var"`, `"$@"` (never bare `$*`), `"$(cmd)"` — unquoted expansions
  word-split and glob, which is the single most common script bug. Filenames
  with spaces are the test case.
- Build command arguments in a bash array (`args=(-x "$file"); cmd "${args[@]}"`),
  not by concatenating a string. Avoid `eval` entirely.
- Prefer `[[ ]]` over `[ ]` in bash (no word-splitting inside); prefer
  `$( )` over backticks.

## Everything can fail

- `cd` can fail — `cd "$dir" || exit 1` (or rely on `set -e`, knowing its
  exceptions). Same for `mkdir`, `rm`, and anything touching the filesystem.
- Check that non-standard tools exist before calling them:
  `command -v jq >/dev/null || { echo "jq required" >&2; exit 1; }`.
- A pipe runs each side in a subshell: `cmd | while read -r line; do
  count=$((count+1)); done` loses `count`. Feed loops with redirection
  (`while read -r … done < file`) or process substitution
  (`< <(cmd)`) when you need to keep variable updates.
- Errors go to stderr (`echo "…" >&2`); exit non-zero on failure so callers
  can react.

## Files and cleanup

- `tmp=$(mktemp)` for temp files, paired with `trap 'rm -f "$tmp"' EXIT` so
  cleanup survives early exits.
- Handle arbitrary filenames end-to-end: `find … -print0 | xargs -0 …`,
  `read -r`, and quoted expansions throughout.

## Clean script structure

- Anything beyond a few lines gets functions: `lowercase_underscore` names,
  `local` for every function variable, constants as `readonly NAME=…` at the
  top, environment/exported variables in `UPPER_CASE`.
- Larger scripts follow the `main` pattern — logic in functions, a `main()`
  that orchestrates, and `main "$@"` as the last line, so nothing executes
  during a partial source or review.
- A script with flags gets a `usage()` printed on `-h`/bad args; parse with
  `while getopts` (bash) rather than ad-hoc `$1` juggling.
- Prefer shell built-ins over spawning processes for simple work: parameter
  expansion (`${file%.txt}`, `${var:-default}`) instead of `sed`/`awk`
  round-trips; `$(( ))` for arithmetic.
- Long-running or destructive scripts log progress to stderr and aim for
  idempotence — re-running after a partial failure should be safe.

## Verify

- `bash -n script.sh` for a syntax gate; then execute the real flow — dry
  reads prove nothing in shell.
- Run `shellcheck` when it's available and fix what it flags; it knows every
  pitfall on this page and more.

## Know when to stop

A script that needs data structures, error recovery, or is growing past ~100
lines of logic has outgrown shell — moving it to a real language is the
correct fix, not more bash.
