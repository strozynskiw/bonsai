# Slash commands

Complete reference for the `/command` surface. **Headless** marks whether
the command also works in `bonsai -p "/command"`; TUI-only commands print an
explanatory message there. Typing `/` in the composer opens the command
completion popup; `Tab` completes names and arguments.

## Help and reports

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/help` | — | command list popup | yes |
| `/keys` | — | keyboard-shortcut popup | yes |
| `/ctx` | — | context-window breakdown (see [Context](context.md#ctx)) | yes |
| `/episodes` | — | task-episode ledger | no |
| `/perf` (alias `/cost`) | — | session performance/usage report | yes |
| `/usage` | — | cross-project usage dashboard (6 tabs + heatmap) | no |
| `/doctor` | `[json] [online]` | runtime diagnostics ([Troubleshooting](troubleshooting.md)) | yes |
| `/bug` | `<description>` | build a local, user-reviewed support bundle | yes |
| `/update` | — | check for the newest signed release and install it in the background; outcome lands in the chat (CLI: `bonsai update [--check]`) | no |

## Conversation and session

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/new`, `/clear` | — | rotate to a fresh session (new cache key; episodes/plan cleared) | yes |
| `/resume` | `[id]` | resume a saved session (bare = latest prior) | no |
| `/sessions` | `[all]` | list task sessions; `all` shows diagnostic identities | no |
| `/forget` | `<id>` | delete a saved session | no |
| `/search` | `<query>` | full-text search across saved messages | no |
| `/retry` | `[shortcut]` | retry the latest user turn, optionally on a shortcut model | no |
| `/goal` | `[status\|pause\|resume\|clear\|<objective>]` | manage a persistent session objective and foreground work | no |
| `/questions` | — | reopen a pending question, preserving its deadline | no |
| `/compact` | `[preview] [target=<tokens>] [fallback\|provider\|deterministic]` | compact context now / preview the plan | yes |
| `/copy` | `[all]` | copy focused row / entire transcript | no |
| `/select` | — | toggle copy mode: release the mouse for native terminal selection (also `Ctrl+G`) | no |
| `/quit` | — | exit | yes |

### Persistent goals

`/goal Implement export with CSV and JSON formats, with tests` establishes an
objective and starts work. `/goal` or `/goal status` shows the objective, state,
stop reason, session usage, and pending work. A later `/goal <objective>` replaces
the objective; ordinary messages provide corrections, additional requirements,
or status requests while preserving it. Use `/goal set <objective>` when the
objective itself is a reserved word such as `pause`.

`/goal pause` interrupts foreground work and prevents further goal continuation.
`/goal resume` explicitly starts another attempt from the existing workspace and
context. `/goal clear` stops goal work and removes the current objective while
retaining history and evidence. Pausing and clearing work while the agent is
busy. A replacement starts after the previous foreground task releases its
slot. Esc and Ctrl+C retain their existing interruption semantics; interrupted
goal work is paused. Esc with staged steering continues the same goal under
the replacement turn. Detached children retain their existing independent
lifecycle. Pausing holds queued steering until resume; replacement or clear
retires it with a transcript notice.

Goals survive compaction, model/provider changes, and session restoration.
Restoring an active goal pauses it; opening a session never starts it silently.
Existing turn, token, cost, and time limits still apply, including cumulative
session usage. An exhausted goal reports a limited state; it cannot be resumed
by bypassing that state. Inspect its limits and evidence before establishing a
new attempt. The agent must explicitly claim that the full objective is complete;
the normal structured completion and verification gates still apply. A progress
report alone does not complete a goal. If repeated reports add no new progress,
Bonsai pauses the goal with a reason; inspect it with `/goal status` and use
`/goal resume` to try again.

In goal mode the `question` tool normally returns immediately so independent
work can continue. Unanswered questions assume the recommended option after
60 seconds; the model may specify a timeout from 1 to 3600 seconds and a
different recommended option. The result states whether the user answered or
the deadline selected an assumption. Press **Tab** to hide a question and
continue typing; `/questions` reopens it without restarting the timer. **Esc**
cancels the question. Before delivering a final answer, the agent waits for any
outstanding background questions. Permission and sandbox prompts always keep
their explicit approval requirements.

## Models and providers

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/model` | `[number \| model \| connection:model \| letter]` | switch the **active mode's** model (bare = 3-pane picker); coding and plan each keep their own selection ([per-mode models](models.md#per-mode-models)) | yes |
| `/<letter>` | — | switch to the model+effort bound to that letter ([shortcuts](models.md#model-shortcuts)) | yes |
| `/authorize` | `[provider]` | authorize a provider (bare = picker; `Ctrl+P` cycles credential store) | yes |
| `/unauthorize` | `[provider]` | remove an authorization | yes |
| `/providers` | `[list\|add\|edit <id>\|remove <id>\|enable\|disable <id>]` | manage connections | yes |
| `/wizard` | `<provider-id>` | alias for `/providers add` | no |
| `/refresh` | — | refresh model catalogs and provider lists | yes |

## Autonomy, safety, posture

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/autonomy` | `ask\|conservative\|balanced\|auto-accept\|yolo\|status` | set the [autonomy level](autonomy-and-permissions.md#autonomy-levels) | yes |
| `/yolo` | `[on\|off\|status]` | bare toggles yolo↔ask | yes |
| `/permissions` | `[list\|remove <id>]` | list rules + authorization ledger; remove a persisted rule | no |
| `/sandbox` | `[on\|off\|net on\|net off\|status]` | [command confinement](sandbox.md) | yes |
| `/self-review` | `auto\|on\|ask\|off\|status` | the [self-review pass](agents.md#self-review) | yes |
| `/mode` | — | runtime-posture picker (autonomy + self-review + sandbox axes) | no |
| `/settings` | — | the settings screen | no |

## Modes, plans, review

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/start` | — | hand the current plan to the coding agent (also `Alt+I`) | expands to the plan workflow |
| `/continue` | — | resume phased plan implementation from the next pending phase | no |
| `/save`, `/discard`, `/new-plan` | — | save / discard / reset the plan canvas | no |
| `/plans` | — | saved-plans picker | no |
| `/export` | `<path>` | export the plan to a file | no |
| `/review` | — | read-only review of pending changes (scope picker: uncommitted / last commit / branch) | no |
| `/security-review` | — | read-only security audit of uncommitted changes | no |
| `/test`, `/build` | — | run the [verification profiles](configuration.md#verification) | yes (`bonsai -p /test`) |
| `/commit` | — | create a git commit | no |
| `/pr` | — | create/update a pull request | no |
| `/pure` | `[on\|off\|status]` | zero-tool conversational persona: the model gets no tools until you turn it off | yes |

## Agents, tasks, peers

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/agents` | `[new\|init]` | combined agent/subagent manager; `new` opens the composer wizard | yes (list/init) |
| `/subagents` (alias `/subtasks`) | — | live subagent view (also `Alt+S`) | no |
| `/tasks` | `[stop <id>]` | list/stop background tasks, PTYs, background subagents (also `Alt+T`) | yes |
| `/peers` | — | live view of other bonsai sessions in this project | no |

## Extensibility

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/config` | `[validate \| edit [project\|global]]` | merged [config](configuration.md) view / re-validate / file paths | yes |
| `/mcp` | `[tools\|enable\|disable\|reload\|revoke <s> \| auth <s> [store] \| callback <s> <url>]` | [MCP servers](mcp.md) | yes |
| `/hooks` | `[enable\|disable\|test <name>]` | [hooks](hooks.md) | yes |
| `/skills` | `[disable\|enable <name>]` | [skill](skills.md) manager | yes |
| `/skill` | `<name>` | load one skill into context | yes |
| `/memory` | `[<name> \| forget <name>]` | [memory](memory.md) manager | yes |
| `/remember` | `[project] <fact>` | pin a memory | yes |
| `/init` | — | generate a starter `AGENTS.md` (won't overwrite) | yes |
| `/lsp` | `restart <path>` | restart the language server that owns `<path>` ([Language intelligence](language-intelligence.md)) | yes |

## Custom commands

A markdown file becomes a reusable prompt: `.bonsai/commands/review-pr.md`
is `/review-pr`. Running it submits the file's body as an ordinary user turn,
so it goes through the normal model, budget and permission path.

```markdown
---
description: Review a pull request
argument-hint: <pr-number>
---
Review pull request $ARGUMENTS. Check correctness first, then tests.
```

- **Where.** `<project>/.bonsai/commands/`, `.claude/commands/`,
  `.agents/commands/`, then `$BONSAI_HOME/commands/` (global). A project
  command shadows a global one of the same name; a `.disabled` file in any of
  these directories suppresses names, as for other
  [resources](configuration.md#resource-directories).
- **Names** come from the file name: lowercase letters, digits, `-` and `_`.
  A name that a built-in uses (including aliases and one-letter model
  shortcuts) is rejected rather than overriding it.
- **Frontmatter** is optional. `description` shows in `/help` and the `/`
  popup (default: the first body line); `argument-hint` shows next to the name.
  `allowed-tools` and `model` are recognized and rejected: bonsai cannot scope
  tools or the model to a single prompt.
- **Arguments.** Every `$ARGUMENTS` is replaced by the text after the command
  name. A body without the placeholder gets the arguments appended as an
  `Arguments:` paragraph. A body is sent as written (no shell or file
  interpolation), and one that expands to another slash command is refused.
- **Trust.** Project commands are inert until the workspace is trusted, like
  project skills and hooks; global commands always load.
- **Surfaces.** In the TUI they appear in `/help` (marked `(custom)`, with any
  skipped files and the reason) and in the `/` popup; files are re-read on
  every slash submit, so edits apply without a restart. A custom command runs
  only while the agent is idle. `bonsai -p "/review-pr 42"` runs one headless,
  and [ACP](acp.md) editors receive them as `available_commands_update`.

## Appearance

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/theme` | `[<name> \| export <name>]` | theme picker / switch / export ([Theming](theming.md)) | no |
| `/serenity` | `[on\|off\|status]` | calm transcript view ([TUI guide](tui.md#serenity-mode)) | no |
| `/smol` | `[on\|off\|status]` | compact small-model profile ([Context](context.md#smol-mode)) | yes |
| `/bonsai` | — | regrow the decorative tree | no |

## Behavior while the agent is running

Invoking a command mid-run classifies it: read-only reports (`/ctx`,
`/tasks`, …) run immediately; lock-free actions (`/update`, `/theme`,
posture toggles) apply immediately; model switches defer until idle; state
changes (`/clear`, `/compact`, `/commit`, `/review`, provider mutations)
open a **busy-command** modal offering queue / open read-only / cancel run /
dismiss. Plain text typed mid-run queues as the next user message
(withdrawable with `Up` on an empty composer).

Built-in command names match case-insensitively; custom command names are
lowercase. `/subtasks` is the one legacy alias.
The single-source registry for all of this is
`src/commands/metadata.rs`; shared TUI/headless handlers live in
`src/commands/handlers/common.rs`.
