# Overview

bonsai is a production-grade, local-first terminal coding agent for macOS and
Linux. It is a single static Rust binary — no runtime, no daemon — that drives
a complete coding loop: inspect → edit → run → diagnose → fix → verify →
review, with high autonomy and measurable quality.

## Mission

Build the most capable and trustworthy **provider-independent** coding agent.
bonsai is a direct peer of tools like Claude Code and Codex CLI, with one
structural difference: providers are data, not code. Nineteen built-in
connections (OpenCode Go, OpenCode Zen, OpenAI API, Codex, Anthropic, MiniMax
API, MiniMax Coding Plan, Z.AI API, Z.AI Coding Plan, Moonshot AI API, Kimi
Coding Plan, Xiaomi MiMo API, Xiaomi MiMo Coding Plan, DeepSeek, Qwen Cloud
API, Qwen Cloud Token Plan, OpenRouter, `openai-compatible`,
`anthropic-compatible`) are
declarative entries in a connections catalog bound to one of three wire
transports, with model metadata (context windows, pricing, reasoning support)
sourced from [models.dev](https://models.dev). You can switch providers
mid-conversation, and adding a vendor on an existing transport is a config
entry, not a code change. See [Providers](providers.md).

## The product contract

| Area | Promise |
| --- | --- |
| Coding loop | The agent advances through inspect/edit/run/diagnose/fix/verify/review without requiring the user to manually restart the loop after a normal failure. |
| Trust | Secrets never enter SQLite; credentials live in a protected file, the OS credential store, or session memory — your choice. Effectful actions are authorized before their first side effect. Sandboxes fail safely and explain their posture. |
| Correctness | Release-gating agent evals and a completion report that states what was and was not verified, derived from bonsai's own ledgers rather than the model's recollection. |
| Breadth | First-class Rust, TypeScript/JavaScript, Python, and Go workflows, with graceful grep/bash fallback for everything else. |
| Surfaces | The TUI, headless/CI mode, the local server, and editor clients over ACP share permissions, budgets, persistence, cancellation, recovery, and completion semantics. |
| Extensibility | Trusted skills, custom agents, hooks, and MCP over stdio/HTTP — including authenticated remote MCP. |
| Operations | Reproducible signed releases, documented upgrades, diagnostics, and tested recovery from interruption and provider failure. |

## Feature map

**The agent.** A per-run turn coordinator drives provider calls and tool
execution. Non-conflicting tool calls run in parallel; conflicting ones
serialize. Provider failures retry with typed errors and `retry-after`
awareness, and a fallback model chain can take over a delegated run. A
self-review pass can re-check the agent's own diff before it reports done.
See [Architecture](architecture.md) and [Agents and modes](agents.md).

**Three controls, kept separate.** *Agent mode* picks the persona and tool set
(coding, plan, review). *Autonomy* picks which actions run without an approval
prompt (`ask` → `yolo`, with a destructive floor below `yolo`). The *sandbox*
picks what a spawned command is physically allowed to do (write confinement +
network denial via Seatbelt or Bubblewrap). See
[Autonomy and permissions](autonomy-and-permissions.md) and
[Sandbox](sandbox.md).

**Tools.** File read/write/edit with read-before-write freshness guards, shell
with risk classification, interactive PTY terminals, background tasks, search
(grep/glob), tree-sitter symbol tools, LSP-backed navigation and diagnostics,
web search and fetch with untrusted-content framing, todos, a plan canvas,
subagent delegation, skills, and persistent memory. See [Tools](tools.md).

**Context.** Cache-friendly context assembly, token accounting per provider
family, compaction, default-on task episodes that archive closed work into
compact cards restorable via `recall`, and a `/ctx` report that breaks the
window down. See [Context management](context.md).

**Persistence.** Sessions, transcripts, usage/cost ledgers, permission rules,
and settings live in SQLite under `$BONSAI_HOME` (default `~/.bonsai`), with
ordered migrations, automatic pre-upgrade backups, and rollback. Sessions
resume across restarts (`bonsai -c`, `/resume`). See [Sessions](sessions.md)
and [Compatibility](compatibility.md).

**Headless.** `bonsai -p "prompt"` runs one non-interactive agent turn with
`text`, `json`, or `stream-json` output under a versioned schema, budget
flags, and meaningful exit codes — the same permission engine as the TUI.
Mutating headless runs execute in detached recovery worktrees so the source
checkout is never touched mid-run. See [Headless mode](headless.md) and
[Recovery worktrees](recovery.md).

**Durable session goals.** `/goal <objective>` records an objective and its
lifecycle (`active`, `paused`, `blocked`, `usage-limited`, `budget-limited`,
`complete`) in SQLite, so work can be paused, resumed, replaced, or cleared
explicitly and the same record is readable by the TUI and by the server.
Completion needs evidence from the run — a typed claim inside a current goal
lease — not the model's optimism. See
[Slash commands](slash-commands.md#persistent-goals).

**Local server (preview).** `bonsai serve` exposes one canonical workspace over
loopback HTTP + Server-Sent Events: session create/list/resume/chat/interrupt,
pending-prompt answers, artifacts, and the goal controls above, behind
short-lived scoped bearer tokens, exact Host/Origin policy, and bounded
admission. It drives the same engine, workspace trust, sandbox, budgets, and
persistence as the TUI — there is no privileged server-only path — and every
response carries an `experimental-v0` marker. See [Server preview](server.md)
and [Server changelog](server-changelog.md).

**Editors (preview).** `bonsai acp` runs bonsai as an
[Agent Client Protocol](https://agentclientprotocol.com) agent, so editors
that speak ACP — Zed natively, VS Code through the ACP Client extension — drive
the same engine over stdio: streamed replies and reasoning, tool cards with
diffs, permission prompts, cancel, thread history, and model and autonomy
selectors. Authority stays in bonsai: threads start at `ask`, the editor can
raise autonomy only up to the level the operator saved, and client-supplied MCP
servers are refused. See [Agent Client Protocol](acp.md).

**Extensibility.** Layered `.bonsai/config.toml`, MCP servers (stdio +
Streamable HTTP + OAuth), lifecycle hooks (shell/HTTP/LLM-judge actions),
skills, custom agents, and custom themes — all discovered from conventional
directories, all gated behind workspace trust when they come from the project.
See [Configuration](configuration.md), [MCP](mcp.md), [Hooks](hooks.md),
[Skills](skills.md).

## What bonsai is not (yet)

Windows support and a stable (v1) server API are planned for after 1.0; a
small built-in web UI for `bonsai serve` is planned sooner. A hosted service,
an SDK, and a plugin marketplace are not planned. The local server and the
editor (ACP) surface ship now as previews, deliberately outside the 1.0
stability contract. See the GitHub
[milestones](https://github.com/strozynskiw/bonsai/milestones) for the live
plan; shipped behavior is documented here and in the top-level README.
