# bonsai documentation

`bonsai` is a terminal coding agent: a single static Rust binary that can
inspect, change, run, debug, verify, and review real software through a
catalog-driven set of LLM provider connections, a tool-based execution loop,
and a ratatui terminal UI. This directory is the reference documentation,
split by topic.

## Orientation

| Document | What it covers |
| --- | --- |
| [Overview](overview.md) | What bonsai is, the product contract, and a feature map |
| [Architecture](architecture.md) | Module layout, startup sequence, the agent loop, and the key abstractions |
| [Getting started](getting-started.md) | Installation, first-run onboarding, and the first task |

## Using bonsai

| Document | What it covers |
| --- | --- |
| [Configuration](configuration.md) | `.bonsai/config.toml` layering, every recognized key, env vars, `/settings`, `/config` |
| [Providers](providers.md) | The built-in connections, wire transports, authentication, and credential storage |
| [Models](models.md) | The model catalog, `/model`, reasoning effort, shortcuts, custom model definitions, pricing |
| [Agents and modes](agents.md) | Agent/plan/review modes, subagents, and custom agent definitions |
| [Tools](tools.md) | Complete reference for every built-in tool the model can call |
| [Slash commands](slash-commands.md) | Complete reference for every `/command` |
| [TUI guide](tui.md) | Layout, keybindings, the composer, transcript features, serenity mode |
| [Sessions](sessions.md) | Session lifecycle, resume, persistence, budgets, and the usage ledger |
| [Context management](context.md) | Token accounting, compaction, task episodes, recall, and `/ctx` |
| [Memory](memory.md) | The persistent user/project memory stores and relevance-gated recall |
| [Theming](theming.md) | Built-in themes, custom palettes, the role contract, terminal color modes |

## Safety

| Document | What it covers |
| --- | --- |
| [Autonomy and permissions](autonomy-and-permissions.md) | Autonomy levels, command risk classification, permission rules |
| [Sandbox](sandbox.md) | OS command confinement: Seatbelt, Bubblewrap, writable roots, network policy |
| [Security model](security.md) | Trust boundaries, workspace trust, untrusted content, credentials, redaction, release signing |

## Extending bonsai

| Document | What it covers |
| --- | --- |
| [Skills](skills.md) | On-demand instruction sets: format, discovery, built-ins |
| [Hooks](hooks.md) | Lifecycle hooks: events, actions, the wire protocol, trust |
| [MCP servers](mcp.md) | Model Context Protocol: stdio/HTTP transports, OAuth, namespacing, gating |
| [Language intelligence](language-intelligence.md) | Tree-sitter symbols, the repository map, managed language servers |

## Automation and operations

| Document | What it covers |
| --- | --- |
| [Headless mode](headless.md) | `bonsai -p` print mode, output formats, event stream, exit codes, CI recipes |
| [Recovery worktrees](recovery.md) | Isolated runs, `bonsai recovery` commands, merge semantics |
| [Eval harness](eval.md) | The deterministic eval suite, graders, live mode, reports |
| [Troubleshooting](troubleshooting.md) | `bonsai doctor`, common failures, and where to look |
| [Compatibility and upgrades](compatibility.md) | The 1.x compatibility contract, upgrade backups, rollback |

## Contributing

| Document | What it covers |
| --- | --- |
| [Development](development.md) | Building, testing, style, adding tools and providers, releasing |

## Reading paths

- **New user:** [Getting started](getting-started.md) →
  [Autonomy and permissions](autonomy-and-permissions.md) →
  [TUI guide](tui.md) → [Slash commands](slash-commands.md).
- **Operator wiring bonsai into CI:** [Headless mode](headless.md) →
  [Recovery worktrees](recovery.md) → [Configuration](configuration.md) →
  [Compatibility](compatibility.md).
- **Security review:** [Security model](security.md) →
  [Sandbox](sandbox.md) → [Autonomy and permissions](autonomy-and-permissions.md) →
  [MCP servers](mcp.md) and [Hooks](hooks.md).
- **Contributor:** [Architecture](architecture.md) →
  [Development](development.md) → the topic doc for the subsystem you touch.

The top-level [`README.md`](../README.md) remains the quick tour;
[`AGENTS.md`](../AGENTS.md) is the working guide for coding agents in this
repository; GitHub
[milestones](https://github.com/strozynskiw/bonsai/milestones) and issues track
unfinished work.
