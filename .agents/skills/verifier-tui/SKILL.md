---
name: verifier-tui
description: Build, launch, and drive the bonsai TUI in an isolated PTY to verify behavior at the real surface — status bar, keybinds, slash commands. Use when verifying a TUI-facing change, or when /verify needs a TUI evidence-capture handle for this repo.
---

# verifier-tui

The bonsai surface is a terminal UI. Unit tests render widgets in isolation;
they do **not** prove the running app wires a keybind/command to the rendered
state (the idle-`/yolo` bug slipped past green tests for exactly this reason).
This skill launches the real binary under the agent's own `bash interactive:true`
+ `terminal` tool — no tmux dependency — and captures the vt100-parsed screen,
so a reviewer can replay what was observed.

## Launching

Build first, then start bonsai in an interactive PTY with the same isolated
environment the `e2e/` suite uses (no network, no real keys, a seeded
authorized provider so the TUI opens to chat, not the setup wizard):

```sh
cargo build
```

Then in the agent, use `bash interactive:true`:

```
env BONSAI_HOME=$(mktemp -d) BONSAI_DISABLE_MODELS_FETCH=1 \
  OPENAI_COMPATIBLE_BASE_URL='http://127.0.0.1:9/v1' \
  OPENAI_COMPATIBLE_MODEL='mock-model' \
  OPENAI_COMPATIBLE_API_KEY='e2e-test' \
  ./target/debug/bonsai
```

This returns a `pty-N` ID. Resize it to a workable dimension immediately:

```
terminal { action: "resize", terminal_id: "pty-1", rows: 40, cols: 140 }
```

Wait for the composer meta line to render (the `(Agent|Plan) ·` regex),
polling with `terminal read` every ~1s.

## Driving the TUI

Send keystrokes with the `terminal` tool — `send` writes raw bytes to the PTY
and a terminal send of `M-m` is `\x1bm`:

| Action | terminal send input | append_enter |
|--------|---------------------|--------------|
| Type `/yolo on` then Enter | `/yolo on` | true |
| Alt+M | `\x1bm` | false |
| Tab | `\t` | false |
| Escape | `\x1b` | false |
| Ctrl+C (interrupt) | use `terminal { action: "interrupt", ... }` | — |

After each send, allow a short settle (0.5–1s), then `terminal read` to
capture the updated screen.

### Reading the surface

The `terminal read` output includes a `Normalized screen:` section — the
vt100-parsed pane content. Use it to check:

- **Composer meta line** (bottom): `● Agent · <model> · <effort> · <provider> [· <policy>]`.
  The execution-mode policy marker is appended only when non-default
  (`auto-accept` amber, `yolo` red); `default` is quiet. Note the `· default`
  earlier on the line is the *reasoning effort*, not the policy.
- **Header** (row 0): compact status pill — used on short terminals where the
  composer is hidden (drive with `rows: 8` to force it).
- **Colors**: the screen output is plain text (no ANSI); for color checks use
  `terminal` with raw output or the `ansi`-preserving capture mode described
  below. Match foreground SGR codes `38;2;R;G;B` against the palette in
  `src/tui/theme.rs`.
- **Composer input line**: the row starting with `│ > ` — the LAST such line
  is the active input (completion popups draw additional `│ > …` rows above
  it).

### Wait + poll pattern

The screen doesn't update instantly. After sending a key sequence, poll with
`terminal read` until the expected substring appears (or a timeout elapses).
A ~3s timeout with ~0.5s polls works reliably:

```
send keys → sleep 0.5s → read → check for expected substring → repeat until found or timeout
```

## Worked example: execution-mode regression test

The M3.1 checklist, driven through the terminal tool:

1. **Launch** and wait for `● Agent ·` in the meta line
2. **`/autonomy auto-accept`** → meta line shows `· auto-accept`, no `· yolo`
3. **`/yolo on`** → meta line shows `· yolo`, no `· auto-accept` (this is the regression guard — yolo must not collapse to auto-accept)
4. **`/autonomy ask`** → neither marker (quiet)
5. **`/autonomy yolo`** → meta line shows `· yolo` (yolo selectable directly)
6. **Alt+M** from ask → cycles to `· conservative` (Alt+M walks ask → conservative → balanced → auto-accept; never yolo)
7. **Tab** → switches to `Plan ·` agent

Covers: default is quiet → `/autonomy auto-accept` → `/yolo on` is distinct
(not auto-accept) → `/autonomy ask` clears → `/autonomy yolo` selects directly
→ Alt+M cycles → Tab switches agent.

## Batch testing (CI)

The `e2e/` suite (`e2e/run.sh`) is the committed, batch-runnable version of
this workflow. It uses tmux for CI determinism (tmux guarantees a
ptmx-backed terminal, which some CI runners need). Use it for pre-commit and
CI gates. This skill is the ad-hoc, single-shot `/verify` handle — use the
`terminal` tool for a quick check during development, then port any new
assertion into an `e2e/cases/` script.

## Notes

- Each run launches the real app, which writes a session to `$BONSAI_HOME`.
  The mktemp dir is ephemeral; it disappears when the PTY exits.
- `terminal send` writes raw bytes — there is no key-name abstraction.
  Common escapes: `\x1b` (Escape), `\x1bm` (Alt+M), `\t` (Tab), `\r` (Enter
  without append_enter), `\x03` (Ctrl+C, or use the interrupt action).
- Give the app a beat after launch (~3s to reach chat-ready) and after each
  `send` (~1s) before reading, or you race the render.
- The `Normalized screen:` in terminal read output strips ANSI escapes. If
  you need color data, use a separate approach (the e2e/ suite captures with
  `tmux capture-pane -e` for ANSI).
