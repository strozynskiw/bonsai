# Agents and modes

bonsai distinguishes **agents** (user-facing personas in the main
conversation) from **subagents** (scoped delegations that return only a
conclusion). Both can be extended with markdown definitions — no source
changes.

## Agent modes

An agent mode picks the persona, tool registry, and view:

- **Agent / coding** *(default)* — the full coding tool set: read, edit,
  write, shell, background tasks, interactive PTYs, todos, delegation, and
  project inspection. Shows the TODO sidebar. This is the only persona that
  runs the [self-review pass](#self-review).
- **Plan** — read-only research plus the live **plan canvas** (the 16
  `plan_*` tools). Switch with `Tab`/`Shift+Tab` or `1`/`2`; the
  conversation stays in place, and a mode transition note tells the model
  the persona changed. `/start` (or `Alt+I`) hands the plan to the coding
  agent. Plan-canvas state persists (`/save`, `/plans`, `/export <path>`,
  `/discard`, `/new-plan`).
- **Review** — launched with `/review`; reviews uncommitted changes, the
  last commit, or the current branch versus `main`/`master` with enforced
  read-only tools. `/security-review` runs the stricter security-only audit
  of uncommitted changes through the same read-only registry.

Custom agents with `surface: [mode]` join the `Shift+Tab` persona cycle. A
user-facing agent always runs on the active session model;
`model`/`fallback_model` assignments apply when the same definition is
invoked as a subagent.

## Subagents

A subagent runs a focused sub-task in its **own fresh conversation** with
its own prompt and tool registry, then returns **only its conclusion** to
the parent — internal turns never enter the main context, so delegations
like "map how X works" keep the parent lean. The parent's model sees a
byte-stable **Subagents index** in its prompt and delegates through the
`agent` tool on its own; you can also nudge it ("use the explore subagent to
find where sessions are persisted").

Built-ins (read-only, reserved ids):

| Subagent | Purpose | Budget |
| --- | --- | --- |
| `explore` | breadth-first codebase orientation; returns `file:line` conclusions | 12 turns / 450 s |
| `review` | read-only review of current uncommitted changes | 12 turns / 450 s |
| `research` | depth-first investigation and synthesis | 36 turns / 900 s |
| `security-review` | read-only audit of effects, auth/untrusted-data boundaries, dependencies | 12 turns / 450 s |

Mechanics:

- **Concurrency** — up to 5 subagents run at once; read-only delegations in
  one turn batch in parallel. `run_in_background: true` detaches a
  delegation (`sub-N` in `/tasks`); the parent parks and wakes on
  completion (TUI only — headless treats an unresolved wait as an error).
- **Bounded** — built-ins use fixed compiled budgets; custom subagents
  default to 75 turns (cap 500 via `max_turns`) / 30 min. On exhaustion the
  subagent gets one tools-disabled "conclude now" turn, so even a timeout
  returns a partial conclusion plus its token usage, which folds into the
  session totals shown in `/ctx`.
- **Isolated** — fresh conversation, own read tracker; its file reads don't
  disturb the parent's read-before-write state. Delegated read evidence and
  workspace effects return with the conclusion for attribution.
- **No recursion** — a subagent cannot spawn subagents.
- **Safe by default** — built-ins and custom definitions without `tools:`
  are read-only. Mutating grants pass through the parent's permission gates
  and serialize against conflicting work.
- **`/subagents`** (alias `/subtasks`, or `Alt+S`) — live view of running
  and finished subagents: prompt, status, elapsed, live tool calls, final
  result; `m` sets a session-scoped model override for an agent name.
- **`/agents`** — the combined manager. Every row is tagged `agent`,
  `subagent`, or `both`, plus `built-in`/`project`/`global` origin. Editing
  a built-in changes only its enabled state and model assignments (stored in
  SQLite); its compiled prompt, tools, and limits stay built in.

## Defining your own

Drop a markdown file with YAML frontmatter into an `agents/` directory
(standard [resource precedence](configuration.md#resource-directories):
`.bonsai/agents/` > `.claude/agents/` > `.agents/agents/` >
`$BONSAI_HOME/agents/`; project files need workspace trust; built-in ids are
reserved):

```markdown
---
name: api-explorer
description: Maps HTTP routes and handlers, returns file:line
tools: [read, grep, symbol_search]
surface: [subagent]          # [mode] for Shift+Tab, or [mode, subagent] for both
# model: f                   # a shortcut letter, or codex:openai/gpt-5.5
# effort: low
# fallback_model: anthropic:anthropic/claude-sonnet-4-6
# fallback_effort: medium
---
You are a read-only API exploration subagent. Locate route/handler definitions
with grep and symbol_search and report them as `file:line` with method + path.
Do not modify anything.
```

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | yes | definition id; match the filename; built-in ids reserved |
| `description` | yes | one line for `/agents` and the model's Subagents index |
| `surface` | no | `[subagent]` (default), `[mode]`, or `[mode, subagent]` |
| `tools` | no | grant scope; omit for the read-only default. Grantable: the read-only core plus `write`, `edit`, `apply_patch`, `bash`, `terminal`, `rename_symbol`, `skill`, `question`, `webfetch`. Unknown names are ignored and reported by `/agents`. |
| `model` / `effort` | no | model selector when invoked as a subagent — a shortcut letter (follows the live binding) or a full `/model` selector; omit to inherit the parent model |
| `fallback_model` / `fallback_effort` | no | backup used when the primary is unavailable or fails after normal retries |
| `enabled` | no | default true; disabled definitions stay visible but can't run |
| `view` / `color` | no | presentation for a mode-surface agent |
| `max_turns` | no | custom-subagent turn cap (1–500) |
| `permission` | unsupported | recognized for compatibility but rejected until agent-scoped permission profiles are enforced |

Model resolution order: a live `/subagents` override (session-scoped) →
`model` → `fallback_model` → the parent model. Provider failover is sticky
for the rest of one delegated run and applies only to provider errors —
never to cancellation, permission or tool failures, timeouts, or budget
exhaustion. Built-in read-only lanes cap reasoning effort at `medium`
(downshift only).

`/agents new` opens a composer wizard (details → tools → prompt → review,
with `Ctrl+G` prompt generation); `/agents init` writes an example file.

## Self-review

Before the coding agent reports a task done, it can run a **self-review
pass**: the uncommitted diff (scoped to changes this run actually made —
peer-session edits are subtracted) is handed to a fresh read-only reviewer
subagent, and the critique comes back as harness evidence — "a reviewer
examined your changes; fix anything genuinely wrong, then confirm." If
something is wrong the agent fixes it in place; otherwise it confirms and
stops. At most once per user message; skipped when nothing changed or the
diff is trivial.

`/self-review auto | on | ask | off | status`:

| Mode | Runs |
| --- | --- |
| `auto` *(default)* | follows autonomy — on at `balanced` and above |
| `on` | always |
| `ask` | prompts each time (skipped when noninteractive) |
| `off` | never |

Set a startup default with `BONSAI_SELF_REVIEW=auto|on|ask|off`. Reviewer
disposition (fixed / none-needed / rebutted) and finding counts are recorded
and surfaced in the [completion report](headless.md#the-completion-report).

**Reviewer model.** By default the reviewer runs on the parent model. Pin a
different one — cheaper, or independent for less correlated blind spots — with
`/self-review model`, which opens the model picker; `/self-review model default`
clears it. The choice persists under the `self-review` built-in-subagent
settings id (the same store as `/subagents` assignments), so it survives a
restart. Self-review is **not** a delegatable subagent — it never appears in the
`/subagents` picker and the model can't call it — which is why its model lives
under `/self-review`, next to its mode toggle.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Personas & prompts | `src/agent/persona.rs`, `src/agent/prompts.rs` |
| Subagent registry & live view | `src/subagent.rs` |
| Delegation tool, catalog, runner | `src/tool/agent/` |
| Custom definitions | `src/resource/agent.rs` (discovery: `src/resource/discovery.rs`) |
| Self-review | `src/self_review.rs`, `src/agent/self_review.rs` |
