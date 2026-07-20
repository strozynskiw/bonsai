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

## Conversation and session

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/new`, `/clear` | — | rotate to a fresh session (new cache key; episodes/plan cleared) | yes |
| `/resume` | `[id]` | resume a saved session (bare = latest prior) | no |
| `/sessions` | — | list saved sessions | no |
| `/forget` | `<id>` | delete a saved session | no |
| `/search` | `<query>` | full-text search across saved messages | no |
| `/retry` | `[shortcut]` | retry the latest user turn, optionally on a shortcut model | no |
| `/compact` | `[preview] [target=<tokens>] [fallback\|provider\|deterministic]` | compact context now / preview the plan | yes |
| `/copy` | `[all]` | copy focused row / entire transcript | no |
| `/quit` | — | exit | yes |

## Models and providers

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/model` | `[number \| model \| connection:model \| letter]` | switch model (bare = 3-pane picker) | yes |
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
| `/save`, `/discard`, `/new-plan` | — | save / discard / reset the plan canvas | no |
| `/plans` | — | saved-plans picker | no |
| `/export` | `<path>` | export the plan to a file | no |
| `/review` | — | read-only review of pending changes (scope picker: uncommitted / last commit / branch) | no |
| `/security-review` | — | read-only security audit of uncommitted changes | no |
| `/test`, `/build` | — | run the [verification profiles](configuration.md#verification) | yes (`bonsai -p /test`) |
| `/commit` | — | create a git commit | no |
| `/pr` | — | create/update a pull request | no |

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

## Appearance

| Command | Arguments | Does | Headless |
| --- | --- | --- | --- |
| `/theme` | `[<name> \| export <name>]` | theme picker / switch / export ([Theming](theming.md)) | no |
| `/serenity` | `[on\|off\|status]` | calm transcript view ([TUI guide](tui.md#serenity-mode)) | no |
| `/smol` | `[on\|off\|status]` | compact small-model profile ([Context](context.md#smol-mode)) | yes |
| `/bonsai` | — | regrow the decorative tree | no |

## Behavior while the agent is running

Invoking a command mid-run classifies it: read-only reports (`/ctx`,
`/tasks`, …) run immediately; model switches defer until idle; state
changes (`/clear`, `/compact`, `/commit`, `/review`, provider mutations)
open a **busy-command** modal offering queue / open read-only / cancel run /
dismiss. Plain text typed mid-run queues as the next user message
(withdrawable with `Up` on an empty composer).

Command names match case-insensitively; `/subtasks` is the one legacy alias.
The single-source registry for all of this is
`src/commands/metadata.rs`; shared TUI/headless handlers live in
`src/commands/handlers/common.rs`.
