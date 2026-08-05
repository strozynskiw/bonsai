# TUI guide

The terminal UI is a single-binary ratatui application: transcript, composer,
status surfaces, a plan canvas, and a set of modals. This page is the user
reference; the rendering architecture is summarized at the end.

## Layout

A vertical stack: **header** (1 row) → **main area** → **composer** (4 rows)
+ footer. On terminals shorter than 18 rows the composer hides and status
moves into the header. The main area depends on the active view:

- **Chat** — transcript only.
- **Agent (todo) view** — transcript plus a right **TODO sidebar** (on
  terminals ≥104 columns) showing the live task list with phase position.
- **Plan view** — transcript shrinks to the left third; the **plan canvas**
  (sections, tasks, phases, review findings) takes the rest.

The **header** shows the bonsai brand, session id and status, workspace
badges (cwd, git branch, elapsed), and a right-pinned **context chip** — a
fill bar showing context usage percent and cost, colored by pressure (green
< 70%, amber ≥ 70%, escalating to red ≥ 95%). Click it to open `/ctx`.

The **status line** (under the composer) shows: a state dot (spinner while
busy), active agent, model — then only deviations from defaults: reasoning,
`smol`, `serenity`, the autonomy marker (amber rising to red at `yolo`), and
the sandbox posture. Attachment chips, queued-message hints, and waiting
peers appear on the same line. The terminal title mirrors state:
`[!] bonsai · <title>` when a modal needs attention, a spinner while busy.

Switching persona mid-run **never cancels the run** — peeking at the plan
canvas while the coding agent works is always safe. The status line shows
the pending switch as `Running → Selected` (e.g. `Coding → Planning`); the
new persona — and its [per-mode model](models.md#per-mode-models) — applies
when the run finishes or is interrupted, and the next message you send runs
under it.

## Transcript

Item types: user messages, queued user messages, assistant text (markdown
with syntax-highlighted code fences), reasoning summaries, tool cards,
execution groups, command output, diffs/edit cards, todo updates, completion
reports, errors, and blue peer-message cards.

- **Tool cards** show a status glyph (`?` awaiting permission, spinner,
  `✓`/`✗`), per-tool argument previews, live durations, and workflow
  classification (format/lint/test/build/check/git).
- **Execution groups** fold a multi-tool turn into one card with a summary
  row (`done/failed/running`, `+N more tools`); Enter expands, arrow keys
  select a tool inside, Enter opens its detail, `d` opens its diff.
- **Detail views** — Enter on any focused block opens the full scrollable
  text (complete reasoning, full tool output, diff preview).
- **Scrolling** — the view follows the bottom; scrolling up shows a floating
  `↓ N new messages (Ctrl+End)` pill. `Shift+↑/↓` extends keyboard
  selection; `Ctrl+A` selects all.
- **Mouse** — wheel scrolls per region; click/double-click/triple-click
  select by position/word/line; drag extends (with edge auto-scroll);
  release copies the selection once. Clicking a modal scrim dismisses it
  like Esc (prompts deny). Tool cards, the context chip, the jump pill, and
  the scrollbar are all clickable.
- An empty transcript grows a bonsai tree; `/bonsai` replants it.

### Serenity mode

`/serenity` toggles a calm, presentation-only view — behavior, stored
transcript, and reasoning settings are untouched. While the model reasons
you see an animated placeholder instead of streaming thought text; finished
thoughts settle into a bucketed label ("Quick thought" … "Went deep").
Multi-tool groups fold to their one-line summary. Nothing is lost: Enter
opens the full reasoning, and copying copies the real text. The choice is a
global preference and survives restarts.

### Accessibility

`--reduced-motion` freezes spinners and shimmer animations;
`--screen-reader` switches to a linear labelled transcript in the normal
buffer without mouse capture or the alternate screen. Monochrome and
high-contrast theming are covered in [Theming](theming.md).

## Composer

- **Multi-line** editing with wrapping; `Enter` submits, `Alt+Enter` /
  `Shift+Enter` insert a newline. Undo/redo (`Ctrl+Z` / `Ctrl+Y`), word
  navigation and deletion, `Ctrl+A` select-all, history (`Alt+↑/↓` or
  `Ctrl+P/N`, 200 entries).
- **Paste chips** — a paste longer than 3 lines / 200 chars collapses into
  an atomic `[Text N]` chip (images via `Ctrl+V` become `[Image N]`); chips
  expand into tagged blocks on submit. Pastes are secret-scanned: a live
  credential is refused outright. Hard limit 256 KiB.
- **@-mentions** — `@path` fuzzy-completes project files (`@{path with
  spaces}` for the braced form). Mentioned files are injected as a
  structured context section with line numbers and count as reads
  (64 KiB/file, 256 KiB total; hidden/binary/out-of-tree paths refused).
- **Completion popup** — `/` lists commands (ranked, `/model` pinned);
  argument completion covers providers, models, themes, sessions, autonomy
  levels, and more. `Tab` accepts; `Enter` on a fully-typed command submits
  as typed.
- **While the agent runs** — `Enter` queues the draft for the next turn. `Esc`
  stops only the foreground turn, sends the current draft immediately as a
  steer, and continues under a fresh foreground turn; detached subagents keep
  running. With an empty draft, `Esc` only stops the foreground turn. `Ctrl+C`
  cancels the whole run tree. Withdraw a pending message with `Up` on an empty
  composer, or cancel a focused pending item with `Delete`. Busy-incompatible
  slash commands open the busy-command modal
  ([details](slash-commands.md#behavior-while-the-agent-is-running)).

## Keybindings

Keybindings are fixed (no user keymap file). The essentials:

| Key | Action |
| --- | --- |
| `Ctrl+C` | staged: cancel the run tree → arm exit → confirm exit |
| `Ctrl+Q` | quit |
| `Enter` while running | queue the draft for the next turn |
| `Tab` | accept completion or cycle focus: composer → transcript → sidebar/plan |
| `Shift+Tab` | cycle persona (coding → plan → enabled custom agents); each mode brings [its own model](models.md#per-mode-models), and switching never cancels a running turn |
| `1` / `2` (outside composer) | agent / plan view |
| `Alt+M` | cycle autonomy (never lands on yolo) |
| `Alt+I` | implement plan (`/start`) |
| `Alt+T` / `Alt+S` | tasks view / subagents view |
| `Ctrl+G` (or `/select`) | copy mode: release mouse capture for native terminal text selection (toggle; a `⊙ select` marker shows while it's on) |
| `Ctrl+T` | cycle focus pane |
| `Ctrl+End` | jump transcript to latest |
| `Esc` while running | stop the foreground turn, send the draft immediately, and continue; detached subagents keep running |
| `Esc` otherwise | progressive: dismiss popup → clear selection → clear draft → close modal |

Full editing/selection/scroll tables are in `/keys` inside the app.

Modal conventions: `Esc`/`q` close, `Enter` confirms, arrows move,
`PgUp/PgDn` scroll. Permission prompts use `a` (once), `s`/`m` (session),
`p` (project), `d` (deny); sandbox-escape prompts have no project option.
In the model picker, `←/→` switch the provider/model/reasoning panes and a
bare letter in the reasoning pane binds a
[model shortcut](models.md#model-shortcuts). In the `/ctx` ledger view:
`p` pin, `d` drop, `s` stub, `r` restore.

## Modals and pickers

Onboarding wizard, help (`/help`, `/keys`), model picker, theme picker
(live preview), settings, mode picker, all permission prompts (command,
sandbox escalation, web domain, MCP tool, hook trust, question, API key),
agent browser + composer wizard, subagent view, task list, peer list,
`/ctx` context inspector, episodes, usage dashboard, session picker, plan
pickers and confirms, review-scope picker, provider manager + local-model
wizard, MCP servers, skill manager, memory manager + add wizard, sandbox
status, and detail overlays (tool, block, diff, plan finding). Every prompt
modal triggers the `[!]` terminal-title attention marker.

## Rendering architecture (brief)

- **One event loop** (`src/tui/run/event_loop.rs`): draws at 100 ms while
  busy / 1 s idle, polls crossterm at 50 ms, flushes changed session
  snapshots every 500 ms off the input path.
- **Reducer pattern** (`src/tui/app/`): all state mutation flows through
  `AppState::reduce` on the render task, via a chain of sub-reducers (input
  → scroll → transcript → groups → context view → pickers → modal → core).
- **Agent → TUI events**: the agent writes to the `SharedSink` trait; the
  TUI sink forwards every callback (`assistant_delta`, `tool_started`,
  `attempt_discarded`, …) over an unbounded channel into the reducer. A
  discarded provider attempt retracts exactly its streamed cells.
- **Per-item render cache** (`src/tui/widgets/transcript/layout.rs`): items
  are pre-wrapped and cached against item revision stamps, per-frame render
  inputs, and a global fingerprint (width/theme/mode), so painting is
  O(viewport).
- **Terminal robustness**: kitty keyboard enhancements are probed before
  being enabled; a dead controlling terminal is detected three ways (CPU-
  share heuristics on empty polls, a stdin `POLLHUP` pre-check, and a
  watchdog thread that exits a wedged loop) so an orphaned session never
  spins a core.
- **Themes** are semantic-role palettes, runtime-switchable; see
  [Theming](theming.md).
