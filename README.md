# bonsai

`bonsai` is a terminal coding agent with catalog-driven connections including
OpenCode, OpenAI, Codex, Anthropic, MiniMax, Z.AI, Moonshot/Kimi, and
OpenRouter, tool-based execution, and a terminal UI.

> [!WARNING]
> **Early release — use at your own risk.** This is an early version of bonsai, and
> yes, large parts of it were written with the help of coding agents. (It's a coding
> agent — how could it not be?) It ships with real safety measures: a command sandbox,
> configurable autonomy modes, risk classification of shell commands, loop and stall
> detection, and permission prompts for anything destructive. But no set of guardrails
> makes an autonomous agent risk-free. It edits files, runs commands, and talks to
> LLM providers on your behalf — mistakes can and will happen. Run it in version-
> controlled directories, review what it does, keep backups of anything you care
> about, and treat every release before 1.0 as experimental. You use it at your own
> risk.

## Install

### From binaries (recommended)

One line, no Rust toolchain required:

```sh
curl -fsSL https://raw.githubusercontent.com/strozynskiw/bonsai/master/install.sh | sh
```

The installer detects your platform, downloads the matching release archive, and
installs to `~/.local/bin` (override with `BONSAI_INSTALL_DIR`). Every download
is verified against the release's Ed25519-signed manifest before anything is
installed — the archive checksum and the extracted binary checksum both have to
match the signed record. `BONSAI_VERSION=v0.2.4` pins a specific release.

If the install directory isn't on your `PATH`, the installer wires it up for
your shell (zsh, bash, or fish) with a single guarded line sourcing
`~/.bonsai/env` — no manual profile editing, and re-running never duplicates
it. Set `BONSAI_NO_MODIFY_PATH=1` to opt out and get manual instructions
instead. Prefer a shared directory? `BONSAI_INSTALL_DIR=/usr/local/bin` works
when you have write access — but the user-local default is what keeps the
built-in self-updater working without elevated permissions.

Prebuilt binaries cover macOS (Apple Silicon and Intel) and Linux glibc
(x86-64 and arm64).

### From source

Building needs a recent stable Rust toolchain — install one with
[rustup](https://rustup.rs) if you don't have it. Cargo places the `bonsai`
binary in `~/.cargo/bin`, which rustup adds to your `PATH` by default.

Install the latest release:

```sh
cargo install --git https://github.com/strozynskiw/bonsai.git --tag v0.2.4 --locked
```

Or install the latest development state from `master`:

```sh
cargo install --git https://github.com/strozynskiw/bonsai.git --locked bonsai
```

Or build from a local clone:

```sh
git clone https://github.com/strozynskiw/bonsai.git
cd bonsai
cargo install --path . --locked
```

### Updating

Binary installs keep themselves updated: on startup, bonsai checks GitHub for a
newer signed release in the background, verifies it against the same
Ed25519-signed manifest as the installer, and stages it — the update applies on
the next launch. `/update` in the TUI or `bonsai update` from the CLI runs the
same verified install on demand (`bonsai update --check` only reports), and
`[update]` in `~/.bonsai/config.toml` tunes the behavior
(`mode = "auto" | "notify" | "off"`, `pin = "X.Y.Z"` to hold a version).

A source install updates by rerunning the install command with the new tag —
Cargo replaces the installed binary when the version differs. To refresh a
`master` build (or reinstall the same version), add `--force`, since Cargo
otherwise skips versions that are already installed:

```sh
cargo install --git https://github.com/strozynskiw/bonsai.git --locked --force bonsai
```

Check what you're running with `bonsai --version`.

### Supported platforms

Bonsai is developed and tested on macOS 13 or newer (Apple Silicon and Intel) and
Ubuntu 22.04 or newer (x86-64 and arm64). Other glibc-based Linux distributions
may work but are best effort. Windows, musl Linux, BSD, and 32-bit systems are not
supported.

## Quick checks

```sh
bonsai --version
bonsai --help
```

Generate completion scripts from the installed binary so they always match its CLI:

```sh
bonsai completions bash > bonsai.bash
bonsai completions zsh > _bonsai
bonsai completions fish > bonsai.fish
```

Install them in the completion directory used by your shell or package manager.

## First run

Launch `bonsai` in the project you want to work on. A resumable six-step setup guides
new installations through the choices needed for a real first task—no config-file editing
is required:

1. Choose the default credential storage: protected file, OS credential store, or the
   current session only. Individual provider logins can override this default.
2. Authorize a provider using its normal API-key, endpoint, environment, or Codex-cache
   flow.
3. Choose and confirm a model and reasoning effort in the normal model picker.
4. Trust the workspace or keep it restricted. Newly trusted project-owned config, MCP,
   hooks, skills, agents, and instructions activate after restarting Bonsai.
5. Review the detected command-sandbox backend. Confinement is enabled and saved when
   available; unsupported hosts show the missing posture without blocking setup.
6. Send a real prompt. Setup is marked complete only after that agent turn succeeds, so a
   provider or task failure can be corrected and retried.

Press `Esc` to defer setup. Completed checkpoints are saved and the flow resumes at the
first unfinished step on the next launch. Existing installations retain their current
configuration and are not forced through the new wizard.

## Settings

Open `/settings` to change the main runtime preferences without editing configuration
files. These are the labels shown by the current settings screen:

<!-- bonsai:settings:start -->
- Model: `model`
- Autonomy: `level`
- Self-review: `mode`
- Context: `smol`
- Credentials: `default store`
- Appearance: `serenity`, `theme`
- Diagnostics: `support log`
- Per-run budgets: `max turns`, `run time`, `generation`, `output`, `tool time`
- Session budgets: `session turns`, `session output`, `session time`, `session cost`
- Sandbox: `confinement`, `network`
<!-- bonsai:settings:end -->

Every budget is off by default. Choosing a preset opts in; command-line limits still
override saved values for that run.

## Modes, permissions & autonomy

bonsai separates three controls that are easy to confuse:

- **Agent mode** chooses the active persona and tool set.
- **Autonomy level** chooses which actions run without an approval prompt.
- **Sandbox** chooses what spawned shell commands are physically allowed to do.

### Agent modes

- **Agent / coding** is the default mode. It has the full coding tool set:
  read, edit, write, shell, background tasks, interactive PTY terminals, todos,
  and project inspection.
- **Plan** is read-only research plus the live plan canvas. Switch with Tab or
  1/2; the conversation stays in place. `/start` hands the plan to the coding
  agent.
- **Review** is launched with `/review`. It reviews uncommitted changes, the
  last commit, or the current branch versus `main`/`master` with read-only tools.
  `/security-review` runs a stricter security-only audit of uncommitted changes
  through the same enforced read-only tool set.

Commands that require a real TTY can opt into `bash interactive:true`. Bonsai
returns a process-local `pty-N` id; the `terminal` tool can read its redacted
output, send up to 8 KiB of non-secret input, resize it, deliver Ctrl-C, or stop
it. Interactive launch retains the normal Bash authorization, hook, workdir,
timeout, and sandbox policy and cannot be combined with parallel execution,
pipe-backed background mode, or a sandbox escape. PTYs also appear in `/tasks`;
their live screen is normalized for TUI/headless events, and a resumed session
marks any previously running process as lost rather than pretending to reattach.

### Model shortcuts

Open `/model`, focus the Reasoning pane for a model, and press any single ASCII
letter to bind that letter to the selected model + reasoning effort. Pressing a
different letter on the same model + effort replaces the old binding.

Switch with either `/model f` or the bare shortcut command `/f`. Custom-agent
frontmatter can use the same selector (`model: f`), so it follows whatever model
is currently assigned to that letter.

### Autonomy (`/autonomy`)

Set with `/autonomy <level>` or cycle the confined levels with Alt+M. The cycle
never lands on `yolo`; removing the guardrails is always explicit. The current
level shows in the status bar: quiet at `ask`, amber as it rises, red at `yolo`.

bonsai classifies bash commands by risk. The autonomy level auto-runs commands
up to its ceiling and prompts for the rest. Below `yolo`, bonsai also keeps the
project-path guard, the read-before-write guard, and the destructive floor.

| level | auto-runs up to | still prompts / blocks | autonomy guardrails |
|-------|-----------------|------------------------|---------------------|
| `ask` | nothing | every risky command | on |
| `conservative` | read-only (`ls`, `git status`, `grep`) | low, medium, high, destructive | on |
| `balanced` *(default)* | medium (`cargo check/test/fmt/clippy`, builds, `go test/build/fmt`, `npm/yarn/pnpm run`, `git commit`, `make`, `docker`) | high and destructive (`rm`, `git push`, installs, network) | on |
| `auto-accept` | high (`rm`, `git push`, installs, network) | destructive only | on |
| `yolo` | everything | nothing | off |

`/autonomy ask | conservative | balanced | auto-accept | yolo | status`. The
`/yolo` shortcut still works (`/yolo on` -> `yolo`, `/yolo off` -> `ask`).

**The destructive floor** applies below `yolo`: `rm -rf`, force-pushes,
`git reset --hard`, and pipe-into-shell commands such as `curl ... | sh` do not
auto-run. A hard-deny set, including `rm -rf /`, `sudo *`, `dd *`, `mkfs *`, and
`curl|wget ... | sh`, cannot be re-enabled by user permission rules.

**Near-autonomous run:** `balanced` already auto-runs the routine dev loop —
including `cargo check`, `cargo test`, and `cargo fmt` — while
pushes, deletes, installs, and network access still stop for you. Step up to
`auto-accept` to let those high-risk actions through while keeping the project
and destructive-floor guards. Use `yolo` only when you intentionally want no
autonomy guardrails.

### Workspace trust

The first interactive launch in an unknown repository starts in restricted mode
and asks whether to trust that project root. Until trust is recorded and bonsai
is restarted, project `.bonsai/config.toml`, MCP servers, hooks, skills, custom
agents, and steering files remain inert; user-global resources still load.
Headless runs cannot answer the prompt, so they keep the same restricted posture
until an interactive session has trusted the workspace.

### Self-review (`/self-review`)

Before the coding agent finishes a task, it can run a **self-review pass**: it
captures the uncommitted diff and feeds itself one more turn — "review your own
changes against the original request and fix anything wrong" — with the full
conversation and the normal coding tools still available. If it spots a bug,
regression, or half-finished work it fixes it in place; otherwise it confirms and
stops. The pass runs at most once per message you send, and is skipped entirely
when nothing changed.

`/self-review auto | on | ask | off | status`:

| mode | when it runs |
|------|--------------|
| `auto` *(default)* | follows autonomy — on at `balanced`/`auto-accept`/`yolo`, off below |
| `on` | always |
| `ask` | prompts you each time (skipped when noninteractive, e.g. headless) |
| `off` | never |

`auto` ties the pass to autonomous coding: it turns on from the default
`balanced` level upward. Set a startup default with
`BONSAI_SELF_REVIEW=auto|on|ask|off`.

### Task episodes

Task episodes are enabled by default. Topic boundaries close older task spans,
which can be replaced by compact `[Episode archived]` cards when doing so saves
context; the `recall` tool restores archived detail inside an untrusted-data
frame. Set `BONSAI_EPISODES=0` (`false`, `off`, `no`, and `disabled` also work)
only when you need the emergency opt-out.

### Web research

The `websearch` tool returns current titles, source URLs, dates when available,
and bounded snippets through Brave Search, Tavily, or a user-selected SearXNG
instance. Search results are untrusted data; the agent prefers official sources
and hands any result it needs to rely on to `webfetch`. Search never fetches a
result page implicitly, so the search-provider domain and every source domain
go through separate network permission checks.

Configure one provider before startup:

- `BRAVE_SEARCH_API_KEY` (or `BRAVE_API_KEY`) for Brave Search.
- `TAVILY_API_KEY` for Tavily.
- `BONSAI_SEARXNG_URL=https://search.example.com` for a SearXNG instance whose
  JSON response format is enabled.

When more than one is configured, Bonsai uses Brave, then Tavily, then SearXNG.
Set `BONSAI_WEB_SEARCH_PROVIDER=brave|tavily|searxng` to choose explicitly.
The tool supports recency and primary-domain filters, preserves returned URLs
in the transcript/headless event stream, respects sandbox network denial, and
shares WebFetch's per-domain allow/deny rules and public-address validation.

### Security review (`/security-review`)

`/security-review` audits the uncommitted diff without exposing mutation or shell
tools. It checks effect authorization, sandbox/path boundaries, credentials and
redaction, untrusted-content framing, dependency and build changes, process/network
use, migrations/rollback, concurrency, and language-specific code-execution risks.
Findings must include an evidenced trigger, affected trust boundary, impact, and
remediation; ordinary style findings are out of scope. The same curated reviewer is
available as the built-in `security-review` subagent for a scoped delegation.

### Serenity mode (`/serenity`)

A calm, presentation-only view of the transcript — the agent's behavior, the
stored transcript, and the model's reasoning settings are untouched:

- **Thinking stays private.** While the model reasons you see an animated
  `⠋ Thinking...` placeholder instead of the streaming thought text; when the
  thought finishes it settles into `Thought a little bit`, `Thought some`, or
  `Thought a lot` depending on how much reasoning the model produced.
- **Tool groups fold.** Multi-tool execution groups collapse to their one-line
  summary with `+N more tools`; click the row (or press Enter on it) to expand
  back to the normal view. Single tool calls render exactly as usual.
- **Nothing is lost.** Press Enter on a thinking block to read the full
  reasoning in the detail view, and copying a block copies the real text.

Bare `/serenity` toggles the mode; `/serenity on|off|status` also work, and the
row in `/settings` does the same. The choice is a global preference stored in
the local database, so it survives restarts and applies to every session.

### Command sandbox (`/sandbox`)

The sandbox is separate from autonomy. Autonomy decides whether a command may run
without asking; the sandbox decides what that spawned command can do if it runs.
By default, it wraps foreground and background bash commands whenever an OS
backend is available. It confines **writes** to approved roots and denies
network egress unless you explicitly allow it.
Reads stay broad so normal toolchains can still inspect SDKs and system files.

The sandbox confines only the child command, not the bonsai process itself. It is
also independent of `yolo`: if the sandbox is on, `yolo` does not turn it off.
Leaving the sandbox is an explicit `/sandbox off`.

Current backend support:

- macOS: Seatbelt through `/usr/bin/sandbox-exec`.
- Linux: Bubblewrap through `/usr/bin/bwrap` when the startup probe confirms
  namespace creation works.
- Other platforms: no active backend yet, so `/sandbox on` reports that the
  sandbox is unavailable.

Commands:

- `/sandbox` or `/sandbox status` opens the sandbox status modal.
- `/sandbox on` enables command confinement when a backend is available.
- `/sandbox off` disables command confinement.
- `/sandbox net on` denies network egress.
- `/sandbox net off` allows network egress.

Writable roots are resolved once at startup:

- the project root
- a private per-session temp directory (exported as `$TMPDIR`, `$TMP`, and `$TEMP`)
- shared dependency stores that cannot be cleanly overlaid (`CARGO_HOME` and an
  explicitly configured `GOMODCACHE`); this is the deliberate compatibility
  exception to per-session cache isolation
- npm, XDG, pip, and Go build caches are redirected below the private session
  temp directory; rustup stays readable but is not writable, so toolchain
  installation/update requires an approved sandbox escape
- extra roots from `BONSAI_SANDBOX_WRITABLE_ROOTS`

Environment defaults:

- `BONSAI_SANDBOX=1|on|true|yes|enabled` starts with the sandbox enabled when a
  backend is available. `0|off|false|no|disabled` disables it. Default: on when available.
- `BONSAI_SANDBOX_NETWORK=deny|denied|block|off` starts with network denied.
  `allow|allowed|on` allows it. Default: denied.
- `BONSAI_SANDBOX_WRITABLE_ROOTS=/path/a:/path/b` adds colon-separated writable
  roots for the session.

### Release diagnostics (`bonsai doctor`, `/doctor`)

Run `bonsai doctor` outside the TUI, or `/doctor` inside it, to check the local
release-critical runtime without contacting a provider. The report covers database
migrations/integrity/write access, config and model-catalog loading, selected-provider
authorization and model selection, active sandbox enforcement, Git and GitHub CLI,
the workspace's relevant language server, MCP configuration, native PTY allocation,
and the installed bonsai build. The sandbox check runs a permitted control write and
verifies that the active backend blocks a write outside every configured writable root.

Every warning or failure includes a concrete next action. `bonsai doctor --json` and
`/doctor json` emit the same versioned, credential-redacted machine-readable report;
the CLI exits non-zero when a required check fails. The default remains fully offline.
Add `--online` to `bonsai doctor`, or `online` to `/doctor`, to explicitly send bounded,
read-only discovery requests to the selected provider and every enabled MCP server and to
verify signed release/update state. Those probes report only service names, versions, and
pass/fail state; response bodies, credentials, URLs, and local paths are excluded.

### Support bundles & lifecycle log (`/bug`, `bonsai bug`)

`/bug <description>` builds a local, single-file support bundle at
`~/.bonsai/support/bug-<timestamp>.md`. Nothing is transmitted anywhere: a review
step lists every section before a byte is written — an offline doctor report, the
last run's completion summary, recent authorization decisions, and usage/budget
state are on by default; the redacted session-log tail is **off** unless you check
it. Raw prompts, code, credentials, and environment values are never included, and
every section (plus the whole document) passes secret redaction. Outside the TUI,
`bonsai bug --description "<text>" [--include-log]` writes the same bundle with the
default sections and no prompt.

The optional **support log** (`/settings` → Diagnostics) additionally records a
redacted JSONL of lifecycle events — turns, guards, context changes, session
transitions — beside the session logs. It is off by default, writes no file at all
until enabled, and its tail becomes an includable bundle section once it exists.

### Maintainer release signing

The release workflow requires a repository variable named
`BONSAI_RELEASE_PUBLIC_KEY` containing the base64-encoded raw 32-byte Ed25519 public key
and a repository secret named `BONSAI_RELEASE_PRIVATE_KEY` containing the matching PEM
private key. The workflow refuses to publish when either value is missing or mismatched;
the private key is never written to the repository or release assets.

### Permission rules (`/permissions`)

When a command prompts, you choose:

- **A** — allow once
- **S** — always for this session (in-memory)
- **P** — always for this project (persisted; survives restarts)
- **D / Esc** — deny once
- **N** — never for this project (persists a *deny*; the command is refused
  automatically next time, no re-prompt)

`/permissions` opens an interactive manager: a searchable list of every editable
rule (bash commands + web domains, session *and* persisted). Type `/` to filter
by pattern, scope, or decision, `d` (or Delete) to remove the selected rule, Esc
to close. It's the quickest way to lift a rule you added by mistake — including a
**N**ever deny. Prefer scripting? `/permissions list` prints the same rules as
text and `/permissions remove <id>` deletes one by id. Rules are scoped per
project (plus a global scope) and a user rule can relax a default but never
re-enable the denied-outright set.

## Agents and subagents

An **agent** is a user-facing persona in the main conversation. Enabled agents
with `surface: [mode]` appear in the **Shift+Tab** switcher and can select their
own prompt, tool scope, color, and view. A user-facing agent runs on the active
session model; `model`/`fallback_model` assignments apply when the same
definition is invoked as a subagent.

A **subagent** is a scoped delegation: the agent hands a focused sub-task to a
smaller agent that runs with its own prompt and tool set, then
returns **only its conclusion**. The subagent's internal turns never enter the
main conversation, so delegating work like "map how X works" or "review these
changes" keeps the parent context lean.

Built-in subagents:

- **`explore`** — read-only codebase exploration; returns `file:line`
  conclusions, not file dumps.
- **`review`** — read-only review of the current uncommitted changes; reports
  issues as `file:line`.
- **`research`** — read-only depth-first investigation and synthesis.
- **`security-review`** — read-only audit of effects, auth and untrusted-data
  boundaries, dependencies, and language-specific security risks.

### Using them

The agent delegates on its own through the `agent` tool — the available
subagents (built-in plus any the project defines) are listed in a **Subagents**
index in its system prompt. You can also nudge it directly: "use the explore
subagent to find where sessions are persisted."

- **`/agents`** opens the combined manager. Every row is tagged `agent`,
  `subagent`, or `both`, plus its `built-in`, `project`, or `global` origin.
  Editing a built-in changes only its enabled state and primary/backup model
  assignment in Bonsai's SQLite database. Its compiled prompt, tools, limits,
  and subagent-only identity remain built in.
- **`/subagents`** (legacy alias **`/subtasks`**, or **Alt+S**) opens a live view of running and finished
  subagents — agent, prompt, status, elapsed, the tool calls it is making, and
  its final result — updating while they run, the way `/tasks` shows background
  jobs.

Several subagents can run at once (up to 5 concurrently); the delegating turn
waits for them, and each subagent's token usage is folded into the session
totals shown in `/ctx`.

### Defining your own

Drop a markdown file with YAML frontmatter into an `agents/` directory — no
source changes, discovered at startup:

- `.bonsai/agents/<name>.md` — project (preferred)
- `.claude/agents/<name>.md`, `.agents/agents/<name>.md` — cross-tool locations
- `$BONSAI_HOME/agents/<name>.md` (default `~/.bonsai/agents/`) — global, shared
  across all your projects

A project definition wins over a global one, and within a project
`.bonsai` > `.claude` > `.agents`. Built-in subagent ids (`explore`, `review`,
`research`, and `security-review`) are reserved and cannot be replaced by a
custom prompt. Legacy same-name files are read only as model/enable settings
until a database-backed setting is saved; afterward they are ignored and kept
as inert compatibility backups rather than deleted automatically.

```markdown
---
name: api-explorer
description: Maps HTTP routes and handlers, returns file:line
tools: [read, grep, symbol_search]
surface: [subagent] # [mode] for Shift+Tab, or [mode, subagent] for both
# Optional: use an assigned model shortcut, or a full /model selector.
# model: f
# model: codex:openai/gpt-5.5
# effort: low
# Optional backup used only after the primary exhausts provider retries.
# fallback_model: anthropic:anthropic/claude-sonnet-4-6
# fallback_effort: medium
---
You are a read-only API exploration subagent. Locate route/handler definitions
with grep and symbol_search and report them as `file:line` with method + path.
Do not modify anything.
```

The markdown body is the subagent's prompt. Frontmatter fields:

| field | required | meaning |
|-------|----------|---------|
| `name` | yes | the definition id shown by `/agents`; match it to the filename. Built-in subagent ids are reserved. |
| `description` | yes | one line shown in `/agents` and, for subagents, the model's Subagents index. |
| `surface` | no | invocation surface: `[subagent]` (default), `[mode]` (Shift+Tab agent), or `[mode, subagent]` (both). |
| `tools` | no | tool scope; omit for the read-only default. The composer lists every grantable tool. Mutating tools still pass through the parent permission policy. |
| `model` | no | model selector used when invoked as a subagent: a one-letter shortcut such as `f`, or a full `/model` selector such as `codex:openai/gpt-5.5`. Omit to inherit the parent model. A Shift+Tab agent uses the active session model. |
| `effort` | no | reasoning effort for the selected model (`minimal`, `low`, `medium`, `high`, `xhigh`, `max`). |
| `fallback_model` | no | backup selector used when the persisted primary is unavailable or fails after its normal retry policy. |
| `fallback_effort` | no | reasoning effort for `fallback_model`; omit for that model's default. |
| `enabled` | no | `true` by default; disabled definitions remain visible in `/agents` but cannot run. |
| `view` / `color` | no | presentation settings for a user-facing agent. |
| `max_turns` | no | bounded custom-subagent turn limit (1–500). |
| `permission` | unsupported | recognized for compatibility but rejected until agent-scoped permission profiles are enforced. |

Persisted assignments resolve in order: `model`, then `fallback_model`, then
the parent model. A live `/subagents` model override is session-scoped and
replaces the persisted chain for that session. Provider failover is sticky for
the rest of one delegated run and does not apply to cancellation, permission or
tool failures, timeouts, or local budget exhaustion.

### What subagents can and can't do

- **Safe by default.** Built-ins and custom definitions without `tools:` are
  read-only. A custom subagent may explicitly request mutating tools such as
  `write`, `edit`, or `bash`; those effects use the same permission gates as the
  parent and serialize against conflicting work.
- **Scoped.** `tools:` narrows or expands the custom definition's grant from the
  supported set. Unknown tools are ignored and reported by `/agents`.
- **Bounded.** Built-ins use fixed compiled budgets; custom subagents use a
  bounded default or `max_turns` override and cannot recurse — a subagent can't
  spawn subagents.
- **Isolated.** A subagent runs in its own fresh conversation; only its final
  text crosses back, and its file reads don't disturb the parent's state.

## Memory

bonsai keeps a persistent memory so it learns you and your project instead of
re-asking. One fact per markdown file with typed frontmatter, in two stores:

- **user tier** — `$BONSAI_HOME/memory/` (defaults to `~/.bonsai/memory/`),
  follows you across projects: preferences, standing corrections.
- **project tier** — `<project>/.bonsai/memory/`, git-tracked so it ships with
  the repo: goals, constraints, decisions the code doesn't record.

Capture is automatic: when the agent learns a durable fact it saves it with the
`memory_write` tool, surfaced as a normal file diff plus a `remembered: …`
note. Pin one explicitly with `/remember <fact>` (add `project` for the project
tier). `/memory` lists entries, `/memory <name>` shows one (the printed path is
the edit affordance — memory files are plain markdown), `/memory forget <name>`
prunes one.

Injection is relevance-gated: full entries are recalled into a turn only when
hybrid BM25 + embedding retrieval matches the fresh user message — tagged as
background data, never instructions, at most a few hundred tokens. The
byte-stable one-line index is shown in diagnostics and memory commands, but is
not placed in the system prompt because project memory can come from a cloned
repository. The embedding model (~30 MB, `minishlab/potion-base-8M`) downloads
once into `$BONSAI_HOME/cache/memory-model/` the first time a non-empty store
needs it; set `BONSAI_MEMORY_EMBEDDINGS=off` (or build without the
`memory-embeddings` feature) to stay BM25-only.

If your project's `.gitignore` ignores `.bonsai/`, un-ignore the memory store
so project memory actually ships with the repo (bonsai never edits your
`.gitignore` itself):

```gitignore
.bonsai/
!.bonsai/memory/
```

## Language intelligence

`symbol_search`, `read_symbol`, and the startup repository map use built-in
tree-sitter grammars for Rust, TypeScript/TSX, JavaScript/JSX, Python, and Go. They
work offline and do not require a language server.

Definition, references, hover, workspace-symbol, rename, and file diagnostics
use a managed server when its command is installed:

- Rust: `rust-analyzer` (`rustup component add rust-analyzer`)
- TypeScript/JavaScript: `typescript-language-server --stdio`
  (`npm install -g typescript-language-server typescript`)
- Python: `pyright-langserver --stdio`
  (`npm install -g pyright`, or `pip install pyright`)
- Go: `gopls` (`go install golang.org/x/tools/gopls@latest`)

Override command locations with `BONSAI_RUST_ANALYZER`,
`BONSAI_TYPESCRIPT_LANGUAGE_SERVER`, `BONSAI_PYRIGHT_LANGSERVER`, or
`BONSAI_GOPLS`. If a server is absent, bonsai reports one setup hint and leaves the built-in
tree-sitter/search path available.

## Configuration

bonsai reads a layered `.bonsai/config.toml` — no source changes, just edit
the file and relaunch (or run `/config validate` to check it in place first).
This is the substrate later extensibility (MCP servers, hooks) builds on.

You can also just ask bonsai: the built-in `customize` and `provider-setup`
skills teach the agent every extension surface — "add my Ollama models",
"make me a darker theme", "write a deploy skill" — and their examples are
CI-checked against the real schemas so they can't drift from the binary.

- **project** — `<project>/.bonsai/config.toml`, git-trackable so it ships
  with the repo.
- **global** — `$BONSAI_HOME/config.toml` (defaults to `~/.bonsai/config.toml`),
  applies across every project.

Precedence, highest first: `CLI > env > .env > project > global > defaults`.
`.env` is loaded automatically and never overrides an already-set environment
variable, so "env beats .env" needs nothing from you. `BONSAI_CONFIG=<path>`
redirects which file is read for the project layer (a `--config` flag is
planned; the env override covers the same need today). Within a section,
entries merge per name: a project `[mcp.servers.x]` replaces a global one of
the same name wholesale, and `[[hooks]]` concatenate — global first, then
project — with a same-named project hook shadowing a global one.

Every top-level section is optional; a file states only what it overrides. A
starter:

```toml
# .bonsai/config.toml — project config; ~/.bonsai/config.toml has the same
# shape and applies globally.
schema_version = 1

[sandbox]
deny_network = true       # default posture; set false to allow network in the sandbox
writable_roots = []       # extra paths, unioned with BONSAI_SANDBOX_WRITABLE_ROOTS

[verification]
# Omit a lane to detect it from project manifests; an empty list disables it.
test = ["cargo test --locked"]
build = ["cargo build --release --locked"]
after_edit = "off"        # default; off | ask | on
```

- **Workspace concurrency** — reads are optimistic and never wait for a
  workspace lease. Mutations still serialize across Bonsai sessions, validate
  current file content, and use an owner-specific fencing token before the
  final rename/delete. A stale mutation fails with a re-read-and-retry hint.
- **`verification`** — ordered command overrides for `/test` and `/build`.
  Cargo, Node package scripts, Go modules, and Python projects are detected
  from their manifests in stable order; `Cargo.lock` adds `--locked`.
  Commands execute through the agent's normal Bash
  permission, sandbox, hook, cancellation, and transcript-evidence path.
  `after_edit` defaults to `off`. Set `after_edit = "on"` to run the detected
  test profile once after a coding turn that changed workspace state (falling
  back to build when no tests exist). `ask` prompts in the TUI and skips on
  noninteractive surfaces.
  In headless mode use `bonsai -p /test` or `bonsai -p /build`; the slash
  request expands to the same bounded profile workflow before the agent runs.
  JSON output includes the typed check results and final-workspace freshness;
  stale, failed, interrupted, or incomplete verification exits non-zero.
- **`/config`** — the merged view: every configured MCP server and hook, each
  annotated with the layer that won (`global`/`project`/`env`) and its file
  path, plus any load diagnostics.
- **`/config validate`** — re-parses both layers without restarting and prints
  diagnostics, so you can fix a typo and check it without relaunching.
- **`/config edit [project|global]`** — prints the file path to open.

A malformed entry never fails startup: one bad `[mcp.servers.x]` or `[[hooks]]`
table degrades to a diagnostic naming the entry and the field (shown in
`/config`), and the rest of the file still applies. `schema_version` guards
forward compatibility — a file written by a newer bonsai still loads
best-effort, with a warning. The supported 1.x schema, upgrade, backup, and
downgrade rules are defined in the [compatibility guide](docs/compatibility.md).

## Skills

Skills are reusable instruction sets loaded on demand without expanding every prompt.
Put each skill in its own directory:

- `.bonsai/skills/<name>/SKILL.md` — project-specific, highest precedence.
- `.claude/skills/` or `.agents/skills/` — compatible project locations.
- `$BONSAI_HOME/skills/<name>/SKILL.md` — reusable across projects.

```markdown
---
name: deploy
description: Build, verify, and publish this project
---
Follow the repository release runbook. Stop before an external write unless it is
authorized by the current permission policy.
```

Project skills activate only after the workspace is trusted and Bonsai restarts. `/skills`
lists loaded definitions and built-in status; `/skill deploy` loads one explicitly, while
the agent can select a relevant skill through its `skill` tool. Use `/skills disable
<built-in>` or `/skills enable <built-in>` to change a built-in for the current project.
A higher-precedence definition with the same name shadows the lower one.

## MCP servers

bonsai connects to [Model Context Protocol](https://modelcontextprotocol.io)
servers over stdio (a spawned child process) or Streamable HTTP, discovers
their tools at startup, and exposes each one as a namespaced tool the model
can call like a built-in. Remembered MCP approvals are bound to the tool's
declared capabilities and input schema, so a changed declaration prompts again.

```toml
# A local filesystem server over stdio; a remote one uses `transport = "http"`
# with a `url` (and optional OAuth). Declared capabilities set the risk tier.
[mcp.servers.filesystem]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "./docs"]
capabilities = ["read"]          # declared read-only ⇒ lower risk tier
```

See **[docs/mcp.md](docs/mcp.md)** for HTTP/OAuth transports, `${VAR}` header
expansion, `allow_tools`, `batching`, `timeout_secs`, and the full field
reference.

- **`/mcp`** — status for every configured server (state, provenance,
  capabilities, tool count). **`/mcp tools <server>`** lists discovered tool
  names and descriptions. **`/mcp enable\|disable <server>`** toggles for the
  session. **`/mcp reload <server>`** drops and reconnects.
- **`/mcp auth <server> [file|keyring|session]`** — discover the remote
  server's OAuth metadata, use its configured public client id or dynamically
  register bonsai, and print the PKCE authorization URL. The optional storage
  choice overrides the default selected during onboarding for this login. A
  loopback callback completes an ordinary local-browser flow automatically.
  When the browser is on another machine or device, copy its final redirect
  URL and use **`/mcp callback <server> <redirect-url>`** in the original
  bonsai process.
- **`/mcp revoke <server>`** — use the authorization server's token-revocation
  endpoint when advertised, then always remove the local login. Access-token
  expiry refreshes automatically when a refresh token is available; a failed
  refresh leaves only that server visibly failed and points back to `/mcp
  auth`.

OAuth follows the MCP protected-resource and authorization-server discovery
flow, requires S256 PKCE, and sends audience-bound resource parameters. Tokens
are stored through the same protected file, OS credential store, or
session-only boundary as provider credentials—never in SQLite or
`.bonsai/config.toml`. A static `Authorization` header remains supported and
takes precedence for that server; remove it before switching the entry to
OAuth. See the [MCP authorization
specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization).

### What MCP tools can and can't do

- **Namespaced.** A server's tools register as `mcp__<server>__<tool>` on the
  wire and are addressable as `mcp.<server>.<tool>` in permission rules and
  `/mcp`'s output. A name collision with a built-in (or another server)
  degrades that one tool visibly rather than shadowing or crashing.
- **Always untrusted.** Every result — success or error — is framed as
  untrusted data before it reaches the model, the same treatment as fetched
  web pages: a compromised or malicious server can hand back text, never
  instructions the model will follow.
- **Gated like everything else.** Calls go through the same permission gate
  as bash/file writes, driven by the declared `capabilities` (risk tier) and
  your autonomy level — undeclared capabilities default to the most
  cautious posture. Permission rules for MCP tools use `kind = mcp` and match
  dotted ids (`mcp.filesystem.*`), a separate namespace from bash/domain
  rules.

## Hooks

A hook runs a shell command, an HTTP request, or a one-shot LLM judge around
a lifecycle event — formatting a file after every write, blocking edits to
`.env`, auditing every bash call, or gating a risky change behind a fast
model's review.

```toml
# Format Rust files after every agent write/edit (non-blocking, fail-open).
[[hooks]]
name = "cargo-fmt"
event = "PostFileWrite"
matcher = { path = "**/*.rs" }
action = { type = "shell", command = "cargo fmt -- \"$BONSAI_FILE_PATH\"" }
timeout_secs = 20

# Hard-block any agent write to .env files (blocking, fail-closed).
[[hooks]]
name = "protect-dotenv"
event = "PreFileWrite"
matcher = { path = "**/.env*" }
blocking = true
on_failure = "block"
action = { type = "shell", command = "echo 'writes to .env files are blocked by policy' >&2; exit 2" }

# Desktop notification when the session ends.
[[hooks]]
name = "notify-session-end"
event = "SessionEnd"
action = { type = "shell", command = "osascript -e 'display notification \"bonsai session ended\" with title \"bonsai\"'" }

# Audit every bash invocation to an internal webhook (non-blocking).
[[hooks]]
name = "bash-audit"
event = "PostBash"
action = { type = "http", url = "https://hooks.internal.example/bonsai-audit", headers = { X-Auth = "${AUDIT_TOKEN}" } }

# LLM gate: a fast-model judge must approve schema-migration edits.
[[hooks]]
name = "migration-review"
event = "PreFileWrite"
matcher = { path = "migrations/**" }
blocking = true
timeout_secs = 45
action = { type = "llm_prompt", prompt = """
You review a proposed file change. Block it only if it drops or truncates data.
Reply with strict JSON: {"decision":"allow"|"block","reason":"..."}.
Change:
{{diff}}
""" }
```

Each hook takes `name`, `event` (one of `SessionStart`, `SessionEnd`,
`PreToolUse`, `PostToolUse`, `PreFileWrite`, `PostFileWrite`, `PreBash`,
`PostBash`), an optional `matcher`, and an `action` (`shell`, `http`, or
`llm_prompt`); `blocking`/`on_failure` decide whether a pre-event hook can veto
a call and whether a broken hook fails open or closed.

See **[docs/hooks.md](docs/hooks.md)** for the full field reference, the
stdin/POST JSON payload and `BONSAI_*` environment variables, the exit-code and
JSON-response decision semantics, and how per-file and multi-file diffs are
delivered to hooks.

- **`/hooks`** — status for every configured hook (state, provenance).
  **`/hooks enable\|disable <name>`** toggles for the session (edit
  `enabled` in config to persist it). **`/hooks test <name>`** fires a hook
  once with a synthetic payload and prints the decision, without waiting for
  a real match.

### Trust

Global-config hooks are pre-trusted — you wrote your own home file. A
project-config hook with a `shell` or `http` action needs a one-time
approval the first time it would run, shown as a permission-style prompt
(action command/URL, once / session / project / deny); editing the command
or URL later re-prompts. `llm_prompt` hooks skip this — they only ever reach
the model provider you've already authorized. Until you answer, `/hooks`
shows the entry as awaiting approval and it does not fire.

## Theming

`/theme` switches the color theme; run it with no argument for a live picker that
previews each theme on the real UI as you move (Enter saves, Esc restores).
bonsai ships 18 built-ins — the originals (`forest`, `ocean`, `paper`, `ember`,
`sakura`, `glacier`, `dawn`) plus Catppuccin (Mocha + Latte), Gruvbox (dark +
light), Nord, Tokyo Night, Solarized (dark + light), Dracula, and high-contrast
dark + light.

Define your own by dropping a `<name>.toml` into a `themes/` directory — no
source changes, discovered at startup and on the next `/theme`:

- `.bonsai/themes/<name>.toml` — project (preferred)
- `.claude/themes/`, `.agents/themes/` — cross-tool locations
- `$BONSAI_HOME/themes/` (default `~/.bonsai/themes/`) — global

A theme names a `"#rrggbb"` color per semantic role; `extends = "<built-in>"`
inherits the roles you don't override. `/theme export <name>` writes the current
palette to a fully-commented starter file. bonsai honors `NO_COLOR`, downsamples
to 256-color when the terminal needs it, and takes a `BONSAI_COLOR_MODE`
override. See [`docs/theming.md`](docs/theming.md) for the full role reference,
precedence rules, and color-mode details.

## Headless print mode

Run one non-interactive coding-agent turn and exit:

```sh
bonsai -p "summarize the repo"
bonsai --print "run cargo test and report failures" --output-format text
bonsai -p "inspect src/main.rs" --output-format json
bonsai -p "run the smoke check" --output-format stream-json
echo "summarize stdin" | bonsai -p
bonsai -c -p "continue the latest session"
```

The TUI and headless adapters share the same permission engine, run/session
budgets, snapshot writer, cancellation token, recovery workspace, provider
failure guidance, and completion-report classifier. Their intentional
adapter-level differences are explicit: headless cannot answer prompts, eagerly
builds its repository context, has no background subagent wake loop, freezes
volatile context for its one-shot run, reads sandbox preferences from config,
and requires a session id before runtime assembly.

`--output-format` defaults to `text`. In text mode, assistant output goes to
stdout and progress/tool messages, the observed-state completion report, and
`Resume: bonsai -c <id>` go to stderr.
`json` prints one final JSON object with `schema_version: 1`, `status`, `output`, `provider`,
`model`, `session_id`, token/cost `usage`, final shared-snapshot
`persistence_duration_ms`, and `completion_report`. The report
contains structured file changes and tool-provided intent, verification and
self-review evidence, authorization counts, caveats, and budget usage; it is
derived from Bonsai's ledgers rather than the model's recollection. A bounded run that stops early
uses `status: "budget_exhausted"` and includes a typed `budget_exhaustion`
object naming the limit that fired.
`stream-json` prints newline-delimited events such as `assistant_delta`,
`reasoning_delta`, `tool_started`, `tool_output`, `tool_finished`, `status`,
`context`, `error`, and `final`. Every event carries the same `schema_version`;
see the [1.x compatibility contract](docs/compatibility.md) before building a
long-lived consumer.

Headless mode uses the saved provider/session state. `BONSAI_PROVIDER` selects
the provider for a new run; if unset, bonsai uses the persisted current
provider, then falls back to `opencode`. On `bonsai -c -p ...`, the resumed
session pins its stored provider/model/reasoning and ignores `BONSAI_PROVIDER`;
`--model <id>` can still override the model for that run. Provider env vars
override saved settings:
`OPENCODE_API_KEY`, `OPENCODE_MODEL`, `OPENCODE_BASE_URL`,
`OPENCODE_ZEN_MODEL`, `OPENCODE_ZEN_BASE_URL`,
`OPENAI_API_KEY`, `OPENAI_MODEL`, `OPENAI_BASE_URL`,
`OPENAI_COMPATIBLE_BASE_URL`, `OPENAI_COMPATIBLE_MODEL`,
`OPENAI_COMPATIBLE_API_KEY`,
`ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `ANTHROPIC_BASE_URL`,
`ANTHROPIC_COMPATIBLE_BASE_URL`, `ANTHROPIC_COMPATIBLE_MODEL`,
`ANTHROPIC_COMPATIBLE_API_KEY`,
`MINIMAX_API_KEY`, `MINIMAX_MODEL`, `MINIMAX_BASE_URL`,
`MINIMAX_CODING_PLAN_API_KEY`, `MINIMAX_CODING_PLAN_MODEL`,
`MINIMAX_CODING_PLAN_BASE_URL`,
`ZAI_API_KEY`, `ZAI_MODEL`, `ZAI_BASE_URL`,
`ZAI_CODING_PLAN_API_KEY`, `ZAI_CODING_PLAN_MODEL`,
`ZAI_CODING_PLAN_BASE_URL`,
`MOONSHOT_API_KEY`, `MOONSHOT_MODEL`, `MOONSHOT_BASE_URL`,
`KIMI_CODING_PLAN_API_KEY`, `KIMI_CODING_PLAN_MODEL`,
`KIMI_CODING_PLAN_BASE_URL`, `OPENROUTER_API_KEY`, `OPENROUTER_MODEL`,
`OPENROUTER_BASE_URL`, `CODEX_MODEL`, and `CODEX_BASE_URL`. Web search
also reads `BRAVE_SEARCH_API_KEY`/`BRAVE_API_KEY`, `TAVILY_API_KEY`,
`BONSAI_SEARXNG_URL`, and `BONSAI_WEB_SEARCH_PROVIDER` at startup.

Existing allow/deny permission rules apply. Commands that would require an
interactive ask prompt are denied in headless mode, and the question tool fails
instead of blocking. Raise autonomy explicitly with `--autonomy ask|conservative|balanced|auto-accept|yolo`,
`--yolo`, or `BONSAI_AUTONOMY=<level>`. Sandbox escape prompts are still denied
in headless mode at every autonomy level.

Use `--model <id>` and `--effort <level>` to select a model and reasoning effort for
one headless run without changing the saved defaults.

Additional CI/script flags:

- `--max-turns <n>` caps agent tool-call iterations.
- `--max-generation-seconds <secs>` caps each provider generation attempt;
  it overrides `BONSAI_MAX_GENERATION_SECONDS` for this run.
- `--max-output-chars <n>` caps combined streamed reasoning and assistant
  output per provider attempt; it overrides `BONSAI_MAX_STREAMED_CHARS` for
  this run.
- `--max-tool-seconds <secs>` caps each individual built-in or extension tool
  execution and cancels a stalled call without cancelling the whole run.
- `--append-system-prompt <text>` appends trusted operator instructions before
  generated project context.
- `--timeout <secs>` cancels the run with exit `124`.
- `--isolation auto|worktree|off` controls the Git recovery boundary. `auto`
  (the default) isolates runs at `balanced`, `auto-accept`, and `yolo`.
- `-p` with no prompt, or `-p -`, reads the prompt from stdin. Use
  `--print=<text>` for prompts that start with `-`.

### Recovery worktrees

Mutating headless runs use a detached managed worktree and start from an exact
snapshot of the source checkout, including tracked, staged, unstaged, and
untracked non-ignored files. The source checkout is not changed during the run.
When it finishes, Bonsai prints a recovery id:

```sh
bonsai recovery list
bonsai recovery inspect <id>
bonsai recovery merge <id>
bonsai recovery keep <id> [branch]
bonsai recovery discard <id>
```

Interactive isolation is opt-in per launch:

```sh
bonsai --isolation worktree
bonsai -c --isolation worktree
```

The TUI reads and edits the managed worktree, but session history, permissions,
plans, peer identity, `/clear`, and `/resume` remain attached to the original
project. The recovery id is visible in the transcript and printed again after
the TUI exits. `--isolation auto` follows the saved autonomy level; omitting the
flag leaves interactive sessions in the source checkout.

`merge` applies the reviewed result to the source working tree without changing
its Git index. It refuses if either the source content or index changed after
the run started; `keep` preserves the result as a branch for manual integration.
An automatic mutating run outside Git refuses to start unless
`--isolation off` explicitly opts out of recovery. Recovery setup also requires
the workspace to have been trusted during an interactive launch, so internal
Git snapshot operations never activate repository filters or hooks before that
trust decision.

The **Budgets** section in `/settings` can persist limits for turns, total run
time, each provider generation, streamed provider output, and each tool call.
Every limit is **off by default**; choosing a preset opts in globally, and an
explicit headless flag overrides the corresponding saved value for that run.
Separate foreground session-turn, session-output, active-time, and cost limits consume the
persisted usage ledger across `-c`/TUI resumes, including output from failed provider retries.
The cost limit uses cache-aware model pricing and stops before the next provider call only
while every contributing turn has exact usage and pricing; completion reports say when an
unknown-cost turn makes exact enforcement unavailable. Generation/output and tool-time call
limits still reset per provider/tool call.
Exhaustion preserves partial work and the resumable session, and records the
typed reason in SQLite instead of reporting a generic failure.

Exit codes:

- `0`: completed successfully
- `1`: agent/provider/tool/runtime failure, or a distinct `budget_exhausted`
  headless result
- `2`: CLI usage error
- `3`: auth/config error
- `124`: `--timeout`
- `130`: interrupted by Ctrl+C
- `143`: interrupted by SIGTERM

CI example:

```sh
BONSAI_PROVIDER=anthropic ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" BONSAI_AUTONOMY=balanced \
  bonsai -p "run the test suite and summarize the result" \
  --max-turns 20 --max-generation-seconds 300 --max-output-chars 120000 \
  --max-tool-seconds 300 --timeout 900 --output-format json
```

## Eval Harness

Run the deterministic internal eval suite:

```sh
bonsai eval --mode mock --json
```

Reports are written under `target/eval/<run-id>/report.json`. See
[`eval/README.md`](eval/README.md) for the suite schema, graders, and live mode.

## Authentication

Use `/authorize` in the TUI to authorize the current provider, or pass a
provider id such as `/authorize anthropic`. Hosted API and coding-plan
connections prompt for their own API key; this includes MiniMax API and Coding
Plan, Z.AI API and Coding Plan, Moonshot AI API, and Kimi Coding Plan. Keys are
stored in protected files under `$BONSAI_HOME/credentials` by default (`0600`
inside a `0700` directory on Unix; user-bound DPAPI encryption on Windows). The
first-run screen and `/settings` can instead select the operating-system
credential store or session-only storage. `Ctrl+P` cycles the same three
choices for one authorization without changing the default. Bonsai's session
database stores only the credential source, never the secret. Codex imports
the local Codex CLI login on each startup.
`openai-compatible` and `anthropic-compatible` prompt for a base URL, optional
API key, and optional model id for local or external compatible servers.

Optional environment variables can bootstrap or override API-key providers on
startup: `OPENCODE_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`,
`MINIMAX_API_KEY`, `MINIMAX_CODING_PLAN_API_KEY`, `ZAI_API_KEY`,
`ZAI_CODING_PLAN_API_KEY`, `MOONSHOT_API_KEY`, `KIMI_CODING_PLAN_API_KEY`, and
`OPENROUTER_API_KEY`. `opencode-zen` uses the
same `OPENCODE_API_KEY` with optional `OPENCODE_ZEN_MODEL` and
`OPENCODE_ZEN_BASE_URL` overrides. The `openai` connection defaults to the
official `https://api.openai.com/v1` endpoint and `gpt-5.6`, with optional
`OPENAI_MODEL` and `OPENAI_BASE_URL` overrides. OpenRouter also supports optional
`OPENROUTER_MODEL` and `OPENROUTER_BASE_URL` overrides. For a local
OpenAI-compatible server, set `BONSAI_PROVIDER=openai-compatible`,
`OPENAI_COMPATIBLE_BASE_URL`, and `OPENAI_COMPATIBLE_MODEL`; the API key may be
empty. Bonsai creates disabled example connector/model files in
`$BONSAI_HOME/providers` and `$BONSAI_HOME/models`.
Trusted workspaces can commit non-secret endpoint and model overrides under
`.bonsai/providers` and `.bonsai/models`; project files take precedence over the
user catalog, while credential values remain user-scoped.

## Troubleshooting

- Run `bonsai doctor` first. Add `--online` only when you want provider, MCP, and
  signed-release probes; use `--json` for a redacted support report.
- Run `/config validate` after changing `.bonsai/config.toml`, MCP servers, hooks, or
  verification commands. One malformed entry is reported without hiding healthy entries.
- If a model cannot start, reopen `/authorize` and `/model`; local compatible servers also
  need their base URL and model id to match the server.
- If confinement is unavailable, `/sandbox` reports the missing backend and next action.
  Autonomy and permission rules still apply, but no OS sandbox is claimed.
- For an isolated run, inspect the recovery id before using `bonsai recovery merge <id>`.
  See the [compatibility guide](docs/compatibility.md) for upgrade backups and rollback.

## Development

### Release

Prepare a release candidate from a clean working tree, without publishing it yet:

```sh
scripts/release.sh 0.2.0-rc.1 --no-push
```

The script updates `Cargo.toml`, `Cargo.lock`, and public install references,
runs the release checks, commits `chore(release): prepare v<version>`, creates
an annotated `v<version>` tag, and can push both to GitHub. Review the local commit
and tag before pushing them. The pushed tag starts the `Release` workflow, which
builds the release assets and currently publishes a GitHub prerelease.

Use `--no-push` to prepare the commit and tag locally without publishing them.

### Pre-commit hook

The repo ships a pre-commit hook at `.githooks/pre-commit` that mirrors the fast
half of CI, so you catch formatting and lint problems before they reach the
pipeline. On every commit it:

- runs `rustfmt` on the **staged** Rust files and re-stages them, then
- runs `cargo clippy --all-targets --all-features -- -D warnings`.

The hook is tracked in the repo but Git doesn't enable it automatically. Point
your clone at the hooks directory once:

```sh
git config core.hooksPath .githooks
```

It needs the `rustfmt` and `clippy` components (bundled with a default `rustup`
toolchain; otherwise `rustup component add rustfmt clippy`). Tests and the
release build are left to CI. Skip the hook for a single commit with
`git commit --no-verify`.

## Contributing and roadmap

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the
development setup, quality bar, and PR conventions. Issues labeled
[`good first issue`](https://github.com/strozynskiw/bonsai/labels/good%20first%20issue)
are self-contained starters.

Direction is tracked as [GitHub milestones](https://github.com/strozynskiw/bonsai/milestones):
the `0.x` line leads to the 1.0 **stability** release (distribution → release
evidence → an experimental server preview for building UIs on top → release
candidate), and `1.x`/`2.0` map out the phases beyond. Issues labeled
`research` are experiments, not commitments.

Security reports go through
[private vulnerability reporting](https://github.com/strozynskiw/bonsai/security/advisories/new),
never public issues — see [SECURITY.md](SECURITY.md).

bonsai is [MIT licensed](LICENSE).
