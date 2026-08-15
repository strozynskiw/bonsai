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

Build first, then start the repository's verifier entry point in an interactive
PTY. The wrapper allocates the state root, scrubs the inherited environment,
disables dotenv/model discovery, seeds only the dead mock provider, and retains
the isolated database plus binary/worktree/exit evidence automatically:

```sh
cargo build
```

Then in the agent, use `bash interactive:true`:

```
./e2e/verifier.sh
```

Do not launch `./target/debug/bonsai` directly for surface verification. The
wrapper refuses a parent/shared `BONSAI_HOME` unless destructive shared-state
testing is explicitly requested with `--allow-shared-state`.

This returns a `pty-N` ID. Resize it to a workable dimension immediately:

```
terminal { action: "resize", terminal_id: "pty-1", rows: 40, cols: 140 }
```

Wait once for a semantic terminal change, then read the normalized screen and
check for the composer meta line (`(Coding|Planning) ·`). Use a bounded wakeable
`terminal wait`; do not poll redraws.

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

After each send, use one wakeable `terminal wait` (up to ~3 seconds), then one
`terminal read` to capture the updated normalized screen. The wait is keyed to
semantic screen versions, so spinner/redraw noise does not cause a polling loop.

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

The screen does not update instantly. Park once on the terminal's current
semantic version, then inspect the screen after the wake or deadline:

```
send keys → terminal wait (wait_seconds: 3) → terminal read → assert
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

- Each run writes only below its wrapper-owned state root. Completed ad-hoc
  evidence is retained under `target/tui-verification/runs/` (newest 20 runs).
- The parent session's terminal send calls and normalized reads are the input
  and screen replay record; the wrapper manifest records binary/worktree
  identity, child exit state, and the isolated database checksum. Batch runs
  persist all of those artifacts together under `target/tui-verification/e2e/`.
- `terminal send` writes raw bytes — there is no key-name abstraction.
  Common escapes: `\x1b` (Escape), `\x1bm` (Alt+M), `\t` (Tab), `\r` (Enter
  without append_enter), `\x03` (Ctrl+C, or use the interrupt action).
- Use wakeable `terminal wait`, never repeated fixed-delay reads, to settle the
  app after launch or input.
- The `Normalized screen:` in terminal read output strips ANSI escapes. If
  you need color data, use a separate approach (the e2e/ suite captures with
  `tmux capture-pane -e` for ANSI).
