---
name: verifier-tui
description: Build, launch, and drive the bonsai TUI in an isolated tmux session to verify behavior at the real surface — status bar, keybinds, slash commands. Use when verifying a TUI-facing change, or when /verify needs a TUI evidence-capture handle for this repo.
---

# verifier-tui

The bonsai surface is a terminal UI. Unit tests render widgets in isolation;
they do **not** prove the running app wires a keybind/command to the rendered
state (the idle-`/yolo` bug slipped past green tests for exactly this reason).
This skill runs the real binary under tmux and captures the rendered pane, so a
reviewer can replay what was observed.

## Prerequisites

- `tmux` (`brew install tmux`). `tui.sh` falls back to `/opt/homebrew/bin/tmux`.
- A configured provider so the TUI opens to the chat view (not the setup
  wizard). The smoke test assumes this.

## The driver: `tui.sh`

A thin wrapper over tmux so driving the app is terse. From the repo root:

```bash
S=.claude/skills/verifier-tui/tui.sh
cargo build                       # produce ./target/debug/bonsai
"$S" start                        # launch in a fresh 140x40 session
"$S" keys "/mode auto-accept" Enter
"$S" line '(Agent|Plan) ·'        # grep the composer meta line
"$S" keys M-m                     # Alt+M  (tmux key syntax: M-=Alt, C-=Ctrl)
"$S" ansi | grep -aoE '38;2;[0-9;]+m'   # fg SGR codes, for color checks
"$S" stop
```

Subcommands: `start [cols rows]`, `keys <tmux-keys...>`, `text`, `ansi`,
`line <regex>`, `stop`. Env overrides: `BONSAI_BIN`, `BONSAI_TMUX_SOCK`,
`BONSAI_STARTUP_WAIT`, `WAIT` (per-keys settle delay, default 1s).

### Reading the surface

- **Composer meta line** (bottom): `● Agent · <model> · <effort> · <provider> [· <policy>]`.
  The execution-mode policy marker is appended only when non-default
  (`auto-accept` amber, `yolo` red); `default` is quiet. Note the `· default`
  earlier on the line is the *reasoning effort*, not the policy.
- **Header** (row 0): compact status pill — used on short terminals where the
  composer is hidden (drive with `start 120 8` to force it).
- **Colors**: `text`/`line` strip color; use `ansi` and match the `38;2;R;G;B`
  foreground SGR against the palette (`src/tui/theme.rs`).

## Worked example / regression test

`smoke-execution-modes.sh` drives the full M3.1 checklist with assertions and
exits non-zero on failure:

```bash
.claude/skills/verifier-tui/smoke-execution-modes.sh
```

Covers: default is quiet → `/mode auto-accept` → `/yolo on` is distinct (not
auto-accept) → `/mode default` clears → Alt+M cycles → Tab switches agent.
Copy it as the template for a new TUI smoke test.

## Notes

- Each run launches the real app, which writes a session to local storage.
  Harmless, but it accumulates sessions.
- tmux `send-keys` sends a quoted argument verbatim; key *names* (`Enter`,
  `M-m`, `Tab`, `Escape`) must be separate arguments.
- Give the app a beat after launch (`start` waits `BONSAI_STARTUP_WAIT`, 3s) and
  after each `keys` (`WAIT`, 1s) before capturing, or you race the render.
