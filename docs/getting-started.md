# Getting started

## Install

**From binaries (recommended)** — one line, no Rust toolchain required. The
installer detects your platform, verifies the download against the release's
Ed25519-signed manifest, and installs to `~/.local/bin` (override with
`BONSAI_INSTALL_DIR`; pin a release with `BONSAI_VERSION=v0.2.0`):

```sh
curl -fsSL https://raw.githubusercontent.com/strozynskiw/bonsai/master/install.sh | sh
```

Update a binary install the same way — the companion script installs only when
a newer release exists:

```sh
curl -fsSL https://raw.githubusercontent.com/strozynskiw/bonsai/master/update.sh | sh
```

**From source** — with Cargo (requires a recent stable Rust toolchain):

```sh
cargo install --git https://github.com/strozynskiw/bonsai.git --tag v0.2.0 --locked
```

Install the latest development state from `master`, or update an existing
source installation (`--force` reinstalls when the version is unchanged):

```sh
cargo install --git https://github.com/strozynskiw/bonsai.git --locked --force bonsai
```

Bonsai is developed and tested on macOS 13+ (Apple Silicon and Intel) and Ubuntu
22.04+ (x86-64 and arm64); other glibc distributions are best effort. Windows,
musl Linux, BSD, and 32-bit systems are not supported. Prebuilt binaries cover
macOS (Apple Silicon and Intel) and Linux glibc (x86-64 and arm64).

### Quick checks

```sh
bonsai --version
bonsai --help
bonsai completions bash > bonsai.bash   # also: zsh, fish
```

## First run

Launch `bonsai` inside the project you want to work on. A **resumable
six-step onboarding** guides new installations to a working first task — no
config-file editing required:

1. **Credential storage.** Choose the default: protected file
   (`$BONSAI_HOME/credentials`, `0600` in a `0700` directory), the OS
   credential store, or session-only. Individual logins can override this
   default later (`Ctrl+P` in the authorization screen cycles the choice).
2. **Provider authorization.** Authorize one provider through its normal
   API-key, endpoint, environment, or Codex-cache flow.
3. **Model selection.** Pick a model and reasoning effort in the normal
   `/model` picker.
4. **Workspace trust.** Trust the workspace or keep it restricted. Project
   config, MCP servers, hooks, skills, agents, and steering files stay inert
   until the workspace is trusted and bonsai restarts. See
   [Security model](security.md#workspace-trust).
5. **Sandbox review.** The detected command-sandbox backend is shown;
   confinement is enabled and saved when available. Unsupported hosts show the
   missing posture without blocking setup. See [Sandbox](sandbox.md).
6. **First prompt.** Send a real task. Setup is marked complete only after
   that agent turn succeeds, so a provider or task failure can be corrected
   and retried.

Press `Esc` to defer setup at any point — completed checkpoints are saved and
the flow resumes at the first unfinished step on the next launch. Existing
installations keep their configuration and are never forced through the
wizard.

## Your first task

With onboarding done, just type. A few things worth knowing on day one:

- **Modes.** `Tab` (or `1`/`2`) switches between the coding agent and the
  read-only plan mode; `/review` starts a read-only code review. See
  [Agents and modes](agents.md).
- **Approvals.** At the default `balanced` autonomy, the routine dev loop
  (builds, tests, formatters, `git commit`) runs unprompted, while pushes,
  deletes, installs, and network access stop for you. `/autonomy` changes the
  level; every prompt offers allow-once / session / project / deny. See
  [Autonomy and permissions](autonomy-and-permissions.md).
- **Files.** Reference files in the composer with `@path` mentions. The agent
  must read a file (this session) before it may edit it.
- **Model switching.** `/model` opens the picker; single-letter shortcuts
  (`/f`) can be bound to model + effort combinations. Switch mid-conversation
  freely — the transcript stays. See [Models](models.md).
- **Session resume.** Every session persists. `bonsai -c` resumes the latest
  one from the shell; `/resume` picks one interactively.
- **Help.** `/settings` shows the main runtime preferences; `/doctor` checks
  the local runtime; `/ctx` explains where your context window is going.

## Where bonsai keeps things

| Path | Contents |
| --- | --- |
| `$BONSAI_HOME` (default `~/.bonsai`) | Everything user-global below |
| `$BONSAI_HOME/bonsai.db` | SQLite: sessions, transcripts, settings, permissions, usage ledger |
| `$BONSAI_HOME/config.toml` | Global [configuration](configuration.md) layer |
| `$BONSAI_HOME/credentials/` | Protected-file credential store |
| `$BONSAI_HOME/{agents,skills,themes,memory,providers,models}/` | Global [resources](configuration.md#resource-directories) |
| `$BONSAI_HOME/backups/` | Automatic pre-migration database snapshots |
| `<project>/.bonsai/` | Project config, resources, and git-trackable [memory](memory.md) |

## Next steps

- Wire it into CI: [Headless mode](headless.md).
- Understand the safety model: [Autonomy and permissions](autonomy-and-permissions.md),
  [Sandbox](sandbox.md), [Security model](security.md).
- Extend it: [Configuration](configuration.md), [Skills](skills.md),
  [Hooks](hooks.md), [MCP servers](mcp.md).
