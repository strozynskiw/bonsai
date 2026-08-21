# bonsai

A fast, extensible terminal coding agent built in Rust. Bonsai can inspect,
edit, run, debug, verify, and review software from an interactive TUI or a
headless command.

> [!WARNING]
> Bonsai is pre-1.0 software. It can modify files, execute commands, and call
> external services. Use version control, review changes, and keep backups.

## Features

- **Terminal-first workflow** — coding, planning, and read-only review agents in
  one responsive TUI.
- **Broad model support** — OpenAI, Codex, Anthropic, OpenCode, MiniMax, Z.AI,
  Moonshot/Kimi, OpenRouter, and compatible endpoints.
- **Real coding tools** — structured file edits, shell and PTY execution, Git,
  diagnostics, web access, todos, plans, and background tasks.
- **Language intelligence** — built-in symbol search for Rust, TypeScript,
  JavaScript, Python, and Go, plus managed LSP tools.
- **Safe execution** — configurable autonomy, permissions, command
  classification, sandboxing, recovery worktrees, hooks, and verification.
- **Extensible by configuration** — custom agents, subagents, skills, themes,
  providers, models, MCP servers, hooks, and project memory.
- **Headless automation** — text, JSON, and streaming JSON output for scripts
  and CI.

## Install

### Homebrew

```sh
brew install strozynskiw/bonsai/bonsai
```

### Installer

```sh
curl -fsSL https://raw.githubusercontent.com/strozynskiw/bonsai/master/install.sh | sh
```

### From source

Requires a recent stable Rust toolchain:

```sh
cargo install --git https://github.com/strozynskiw/bonsai.git --locked bonsai
```

Prebuilt releases support macOS 13+ and Ubuntu 22.04+ on Apple Silicon,
x86-64, and Linux arm64 where applicable.

## Get started

Launch Bonsai inside a version-controlled project:

```sh
cd your-project
bonsai
```

Complete the first-run provider setup, then describe the task in plain
language. For example:

```text
Find the cause of the failing authentication test, fix it, and run the
relevant checks.
```

Use `Tab` to switch between coding and planning. Common commands include
`/model`, `/review`, `/test`, `/settings`, `/resume`, and `/help`.

## Examples

Run one non-interactive task:

```sh
bonsai -p "summarize this repository"
```

Return structured output:

```sh
bonsai -p "run the tests and report failures" --output-format json
```

Resume the latest session:

```sh
bonsai -c
```

Use a specific model for one headless run:

```sh
bonsai -p "review the current changes" --model anthropic:anthropic/claude-sonnet-4-6
```

## Documentation

Full guides and reference documentation are at
**[docs.bonsaicode.ai](https://docs.bonsaicode.ai)**.

- [Getting started](https://docs.bonsaicode.ai/getting-started.html)
- [Providers and models](https://docs.bonsaicode.ai/providers.html)
- [Configuration](https://docs.bonsaicode.ai/configuration.html)
- [Tools and language intelligence](https://docs.bonsaicode.ai/tools.html)
- [Safety and permissions](https://docs.bonsaicode.ai/security.html)
- [Headless mode](https://docs.bonsaicode.ai/headless.html)
- [Development](https://docs.bonsaicode.ai/development.html)

The documentation source also lives in [`docs/`](docs/README.md).

## Development

```sh
git clone https://github.com/strozynskiw/bonsai.git
cd bonsai
cargo test --locked
cargo run
```

See the [development guide](https://docs.bonsaicode.ai/development.html) for
architecture, contribution, build, test, and release details.

## License

[MIT](LICENSE)
