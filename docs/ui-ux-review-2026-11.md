# TUI UI/UX review — 2026-11

Method: bonsai 0.5.1 release build driven in tmux (120x40, then 80x24 down to
50x12) with an isolated `HOME` and a local fake OpenAI-compatible server, so
tool calls, markdown, diffs and permission prompts could be exercised without a
real provider. Screens covered: onboarding, welcome, slash popup, `/help`,
`/keys`, `/settings`, `/model`, `/ctx`, `/authorize`, `/sessions`, a streamed
turn with `bash` + `write`, a permission prompt, and small terminal sizes.
Colour is not visible in text captures, so colour-only cues are flagged where
the text alone is ambiguous. Findings are ordered by value.

## A. Bugs (wrong behaviour)

1. **Composer disappears below 18 rows.** `layout.rs` sets
   `show_input = area.height >= 18`. At 50x16 or 80x16 there is no input and no
   footer; typed text is invisible. Fix: collapse to a one-line composer
   (and drop the header/sidebar first) instead of removing it.
2. **Code fence has an open right edge.** Top and bottom rules of a fenced
   block reach the full width, content rows have no closing `│`, and the rules
   overrun the scrollbar column.
3. **Tool-group header is wrong after completion.** It keeps the `⟳` spinner
   glyph when the group is done, is titled "Edit" for a group that contains a
   `bash` call, prints the absolute path (`/private/tmp/.../hello.py`) instead
   of the project-relative one, and repeats the info already in the rows below
   (`· cmds: bash · +4 -0`).
4. **Loose list spacing.** A tight Markdown list renders with a blank line
   between every item and `•` followed by three spaces.
5. **Welcome text wraps without a hanging indent.** `/mode … auto-accept\n· yolo`
   continues at column 0 of the box; the same happens at every narrower width.
6. **`[[arg]]` double brackets** in the slash popup usage (`[[provider]]`,
   `[[new|init]]`).
7. **Model picker disagrees with the status bar.** Status shows the default
   `qwen3.8-max` (provider not authorized) while the picker lists only the
   authorized providers; selection starts on an unrelated provider.
8. **Unpriced models show `$0/M`** in the model detail line; should read
   "unpriced".

## B. Information density and layout

1. **Right sidebar spends 34 columns on ASCII art when idle.** Replace the
   empty-state bonsai with useful content: files changed this session,
   background tasks, queued messages, or goal status; keep the tree as a small
   footer glyph.
2. **Truncated columns next to unused width.** `/help` truncates `/goal
   [status|pause...` and `/compact preview ta...` while the Action column has
   ~50 spare cells; `/ctx` ledger shows `30 active tool ...` with similar room.
   Size columns from content, not fixed fractions.
3. **Modals are sized to the terminal, not the content.** The authorize form
   is 8 lines of content in a ~30-row box; the Models picker leaves half its
   height empty. Shrink-to-fit with a min/max.
4. **`/keys` is hard to scan.** Several key/action pairs are packed on one
   line with ragged columns, unordered, with two different separators. Use one
   aligned two-column table grouped by Editing / Navigation / Run control /
   Commands.
5. **Empty-state header noise.** `ctx 0% · cost $0 · cache n/a · ↑0 ↓0` before
   the first message; show counters only once there is data.
6. **`/settings` is a 30-row flat list.** Group with collapsible sections
   (Budgets, Session alerts rarely change) and show the active option with a
   marker, not only colour.
7. **Number formatting** is inconsistent: `1000.0k context`, `1000k`, `5000k`,
   `10000k`. Use `1M`, `5M`, `10M`.
8. **Duplicate commands in `/help`:** `/perf` and `/cost` have the same
   description; show one with an alias note.
9. **Session list omits the current session**, so `/sessions` right after
   start looks like it lacks the session you are in.

## C. Feedback and copy

1. `Agent stopped with work still unfinished.` (after denying a permission) does
   not say what was unfinished or what to do. Name the denied tool and offer
   "retry / continue" keys.
2. Provider errors are good (`opencode is not authorized. Run /authorize
   opencode.`) but the footer still shows the unauthorized model as if usable;
   mark it (`⚠ not authorized`).
3. Onboarding is 7 steps full-screen with large empty areas; offer "skip all /
   use defaults" on step 1 and show what each step changes.
4. Chat title is the raw first prompt (`Chat · please use tools to look around`);
   truncate with `…` and prefer the generated session title once it exists.
5. Heading and legend cues rely on colour (`Summary` heading has no marker;
   authorize legend "authorized / not authorized"). Add a glyph (`✓`) so
   `--screen-reader` and monochrome terminals keep the meaning.

## D. Small-terminal behaviour

- 80 cols: sidebar correctly hides; welcome scrolls so the title is cut off —
  anchor to top until the first message.
- 50 cols: header items overlap (`● ready balanced sandbox no-net ✦ bonsai`
  collides with the brand); drop items by priority rather than overlapping.
- See A1 for the missing composer.

## E. What works well (keep)

Permission modal (concise, one-line key legend, A/S/P/D/N), streaming and
markdown tables, diff summary in tool rows, adaptive sidebar hiding, clear
provider authorization error, `/ctx` ledger with sources, arrow-only selection
convention.

## F. Suggested order

1. A1, A2, A3, A5, A6 (contained renderer/layout fixes with unit-testable
   geometry).
2. B2, B3, B7 (column sizing, modal fit, number formatting).
3. B1 (sidebar content), C1, C2.
4. B4, B6, C3 (screens that need design decisions).

## G. Build/link speed (side finding)

The repo sits on an external SSD (`/Volumes/ADATA SD810`). Relinking after
touching `src/main.rs` (single-crate binary, so every change relinks):

| setup | relink |
|---|---|
| target dir on external disk, default dev profile | 27–68 s |
| target dir on internal disk, default dev profile | 8 s |
| internal, `debug = "line-tables-only"` | 7 s |
| internal, `debug = 0` | 7 s |

The disk, not the linker or debuginfo level, dominates (Apple `ld-prime` is
already the default; lld/mold are not available for macOS). Recommendation:
`export CARGO_TARGET_DIR=$HOME/.cache/bonsai-target` (or `build.target-dir` in
`~/.cargo/config.toml`). The release profile (`lto = "thin"`, 1 CGU) costs
~4.5 min per release build by design; use `cargo build` or `cargo test` for
iteration.
