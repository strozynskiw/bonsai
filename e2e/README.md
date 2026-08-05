# e2e — local TUI smoke tests

End-to-end tests that launch the **real `bonsai` binary** under `tmux`, drive it
with keystrokes, and assert on the rendered terminal pane. They catch wiring
bugs that unit/widget tests can't — a keybind or slash command that renders
correctly in isolation but isn't actually hooked up in the running app (e.g. the
idle-`/yolo` regression).

Scope is **UI-surface only**: startup, slash commands, pickers, keybinds,
execution modes, and deterministic turn handoffs. Turn tests use a loopback
mock provider; no external network or real API keys are used.

## Run it

```bash
./e2e/run.sh                              # build once, run every case, summarize
./e2e/run.sh cases/01_execution_modes.sh  # run a single case
```

Exits non-zero if any case fails, so it drops into CI or a pre-push hook.

**Prerequisite:** `tmux` (`brew install tmux`).

## How it stays deterministic

Each case runs against a throwaway, no-dependency environment (`e2e_begin` in
`lib.sh`), torn down on exit:

| Lever | Effect |
| --- | --- |
| `BONSAI_HOME=$(mktemp -d)` | isolated SQLite state; your real `~/.bonsai` is never touched |
| `HOME=$BONSAI_HOME` | isolates legacy config/session discovery too |
| `BONSAI_DISABLE_MODELS_FETCH=1` | no models.dev network fetch; built-in catalog only |
| `OPENAI_COMPATIBLE_BASE_URL` + `_MODEL` | seeds an *authorized* provider; normally a dead address, overridden by turn tests with the loopback mock |

The "chat is ready" signal is the composer meta line `● Coding · … ` (or
`Planning ·`).
`tui_start` blocks on it instead of sleeping a fixed interval.

## Layout

```
e2e/
  run.sh        # build + run all cases + summary
  lib.sh        # isolated env, tmux driver, render-aware assertions
  cases/
    00_startup.sh          # boots to chat; compact header on a short terminal
    01_execution_modes.sh  # /autonomy, /yolo, Alt+M  (idle-/yolo regression guard)
    02_slash_commands.sh   # /help modal, /theme status, bad-arg no-op
    03_model_picker.sh     # /model opens picker, Esc returns to chat
    04_completion.sh       # "/mod" + Tab -> "/model"  (slash-command completion)
    05_quit.sh             # /quit exits the process cleanly
    06_escape_steer.sh     # Esc replacement remains visibly active
  mock_streaming_provider.py # deterministic loopback SSE provider
```

## Writing a new case

```bash
#!/usr/bin/env bash
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "07_my_case: what it checks"

tui_start 140 40 || e2e_done      # launch, wait for chat-ready

tui_keys "/something" Enter        # send keys (tmux syntax: Enter, Tab, M-m, Escape)
expect      "pane shows X"      "X"      # whole-pane substring (polls ~3s)
expect_meta "meta shows Y"      "· Y"    # composer meta line only
forbid      "no Z"             "Z"       # whole pane lacks substring (immediate)
expect_input "input is /model" "/model"  # the composer input line only

e2e_done                           # print PASS/FAIL, exit accordingly
```

Driver verbs in `lib.sh`: `tui_start`, `tui_keys`, `tui_text`, `tui_ansi`
(with color, for theme/SGR checks), `tui_meta`, `tui_input`, `tui_alive`,
`wait_for <regex> [timeout]`. Assertions: `expect`/`forbid`,
`expect_meta`/`forbid_meta`, `expect_input`/`forbid_input`.

Drop the file in `cases/` (named `NN_*.sh`, `chmod +x`) and `run.sh` picks it up.

## Related

`.agents/skills/verifier-tui/SKILL.md` is the ad-hoc, single-shot verification
handle used by `/verify`. This `e2e/` suite is the committed, batch-runnable
evolution of it, with the environment isolation baked in.
