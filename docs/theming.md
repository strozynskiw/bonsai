# Theming

bonsai's colors all live in a single **palette** — a named color per semantic
UI role. `/theme` switches the active palette at runtime; you can also ship your
own palettes as TOML files. This document is the reference for the role
contract, where custom themes load from, and how colors adapt to your terminal.

## Switching themes

- `/theme` — open the **live picker**. Moving the cursor previews each theme on
  the real UI; **Enter** saves it (persisted per session), **Esc** restores the
  theme you started on.
- `/theme <name>` — switch directly by name (tab-completes built-in and custom
  names).
- `/theme export <name>` — write the active palette to
  `.bonsai/themes/<name>.toml` as a fully-commented starter file.

The chosen theme is remembered and restored before the first frame on the next
launch.

## Built-in themes

Seven originals plus eleven popular palettes:

| Theme | Kind | | Theme | Kind |
|-------|------|-|-------|------|
| `forest` (default) | dark | | `catppuccin` (Mocha) | dark |
| `ocean` | dark | | `catppuccin-latte` | light |
| `paper` | light | | `gruvbox` | dark |
| `ember` | dark | | `gruvbox-light` | light |
| `sakura` | dark | | `nord` | dark |
| `glacier` | dark | | `tokyonight` | dark |
| `dawn` | light | | `solarized` | dark |
| | | | `solarized-light` | light |
| | | | `dracula` | dark |
| `contrast` | high-contrast dark | | `contrast-light` | high-contrast light |

The `contrast` / `contrast-light` themes maximize luminance contrast for
accessibility.

## Custom themes

Drop a `<name>.toml` file into a `themes/` directory. The theme's name is the
file stem, lowercased (allowed characters: `a-z`, `0-9`, `_`, `-`). Files are
discovered at startup and re-scanned whenever `/theme` runs, so edits take
effect without a restart.

### Where themes load from (precedence, highest first)

1. `<project>/.bonsai/themes/`
2. `<project>/.claude/themes/`
3. `<project>/.agents/themes/`
4. `$BONSAI_HOME/themes/` (global; defaults to `~/.bonsai/themes/`)

A **project** theme shadows a **global** one of the same name, and a custom
theme shadows a **built-in** of the same name (letting you retune `forest`,
say). The picker tags each custom theme with `· project theme` or
`· global theme` so shadowing is visible.

To disable a theme (e.g. a built-in you never want to see), add its name on its
own line to a `.disabled` file in any `themes/` directory.

### File format

A theme sets a `"#rrggbb"` hex color for each role. Two non-color keys are
optional:

- `blurb` — the one-line description shown in `/theme` (defaults to
  `"custom theme"`).
- `extends` — the name of a **built-in** theme to inherit from. With `extends`,
  you only list the roles you want to change; everything else comes from the
  base. Without `extends`, **every** role is required.

```toml
# .bonsai/themes/midnight.toml
extends = "ocean"
blurb   = "ocean, but darker"

bg    = "#05070c"
panel = "#0b1018"
```

Validation errors name the file and every problem at once — all missing roles in
one message, each invalid color or unknown key individually — so you can fix a
theme in a single pass. At startup, invalid files are skipped with a log warning;
the next time `/theme` runs, the same errors appear in the transcript.

### The role contract

Every role below must resolve to a color (directly or via `extends`). `/theme
export` writes them in this order, each with its description as a comment.

| Role | What it colors |
|------|----------------|
| `bg` | Base background behind every surface |
| `panel` | Primary raised panel surface (frames, modals, transcript) |
| `panel_dark` | Recessed surface for chrome (input box, footers) |
| `border` | Idle frame/border |
| `border_active` | Focused frame/border and default title accent |
| `user_block` | User-message card background |
| `assistant_block` | Assistant-message card background |
| `thinking_block` | Thinking / reasoning card background |
| `tool_block` | Tool-call card background |
| `result_block` | Tool-result card background |
| `edit_block` | File-edit / diff card background |
| `todo_block` | Todo / plan card background |
| `error_block` | Error card background |
| `peer_block` | Inter-agent (peer) chat message background |
| `text` | Primary foreground text |
| `muted` | Secondary text (labels, metadata) |
| `dim` | Tertiary text (hints, disabled) |
| `user` | Accent for user content |
| `assistant` | Accent for assistant content |
| `progress` | In-progress / streaming accent |
| `tool` | Tool-activity accent |
| `edit` | File-edit accent |
| `error` | Error / failure accent |
| `todo` | Todo / plan accent |
| `success` | Success / completion accent |
| `peer` | Inter-agent message accent |
| `selection_bg` | Selected-row / picker fill |
| `added` | Added diff line foreground |
| `added_bg` | Added diff line background |
| `removed` | Removed diff line foreground |
| `removed_bg` | Removed diff line background |
| `lineno` | Diff / code line-number gutter |
| `path` | File-path values in tool cards |
| `command` | Shell-command values in tool cards |
| `syntax_comment` | Code comments |
| `syntax_string` | String / char literals |
| `syntax_number` | Numeric literals |
| `syntax_keyword` | Language keywords |
| `syntax_type` | Type names |
| `syntax_function` | Function-call identifiers |
| `context_system` | Context inspector: system role |
| `context_user` | Context inspector: user role |
| `context_assistant` | Context inspector: assistant role |
| `context_tool` | Context inspector: tool role |
| `context_tool_schema` | Context inspector: tool schema |
| `agent_accent` | Agent-view focused accent |
| `agent_border` | Agent-view idle border |
| `plan_accent` | Plan-view focused accent |
| `plan_border` | Plan-view idle border |

## Terminal color modes

Palettes are authored in 24-bit truecolor. bonsai detects what your terminal can
render **once** at startup and adapts colors up front, so the per-frame path
stays fast. Detection order (first match wins):

1. `BONSAI_COLOR_MODE` — explicit override: `truecolor`, `256`, or `mono`.
2. `NO_COLOR` (any non-empty value) — monochrome: every role falls back to the
   terminal default; bold/italic still apply. (See <https://no-color.org>.)
3. `COLORTERM` = `truecolor` / `24bit` — truecolor.
4. `TERM` contains `256color` — colors are quantized to the xterm-256 palette.
5. Otherwise — truecolor (the status-quo default for unknown terminals).

So `TERM=xterm-256color` **without** a `COLORTERM` (e.g. macOS Terminal.app,
plain tmux) downsamples to 256 colors. If your terminal actually supports
truecolor but doesn't advertise it, set `COLORTERM=truecolor` or
`BONSAI_COLOR_MODE=truecolor`.

`/theme export` always writes the original truecolor values, never the
downsampled ones, so an exported theme is portable.
