# Configuration

bonsai reads a layered `.bonsai/config.toml` — edit the file and relaunch
(or `/config validate` to check it in place first). Config is the substrate
the extensibility surfaces build on: [MCP servers](mcp.md), [hooks](hooks.md),
the [sandbox](sandbox.md) posture, and [verification](#verification)
commands. You can also just ask bonsai — the built-in `customize` and
`provider-setup` [skills](skills.md) teach the agent every extension surface,
with CI-checked examples.

## Layering

Two files, merged with everything else in a fixed precedence:

```
CLI > env > .env > project > global > defaults
```

- **project** — `<project>/.bonsai/config.toml`, git-trackable. Active only
  in a [trusted workspace](security.md#workspace-trust).
  `BONSAI_CONFIG=<path>` redirects which file is read for this layer (the
  override itself comes from the environment, so it works even in an
  untrusted workspace).
- **global** — `$BONSAI_HOME/config.toml` (default `~/.bonsai/config.toml`).
- `.env` is loaded automatically (disable with `BONSAI_DOTENV=0`) and never
  overrides an already-set environment variable, so "env beats .env" needs
  nothing from you.
- There is no `--config` CLI flag yet; the env override covers that need.

Merge rules per section:

| Section | Rule |
| --- | --- |
| `[mcp.servers.<name>]` | keyed by name, **whole-entry replace** — a project entry replaces a same-named global one wholesale |
| `[[hooks]]` | concatenated global-first then project, in fire order; a same-**named** project hook shadows the global one (so `enabled = false` in the project disables a global hook) |
| `[sandbox]` | `writable_roots` union; `deny_network` scalar, higher layer wins |
| `[verification]` | `test`/`build` whole-list, higher layer wins; same for `after_edit` |

## Fault tolerance

Loading is per-entry resilient: one malformed `[mcp.servers.x]` or `[[hooks]]`
table degrades to a diagnostic naming the entry and field, and the rest of
the file still applies. Only a file that fails TOML parsing entirely drops
the whole layer. Unknown top-level keys are ignored with a
forward-compatibility note; `skills`, `commands`, and `providers` are
reserved for future releases. `schema_version = 1` guards forward
compatibility — a file written by a newer bonsai loads best-effort with a
warning. See the [compatibility contract](compatibility.md).

## The schema

Every top-level section is optional; a file states only what it overrides.

```toml
schema_version = 1

[sandbox]
deny_network = true       # default posture; false allows network in the sandbox
writable_roots = []       # extra paths, unioned with BONSAI_SANDBOX_WRITABLE_ROOTS

[verification]
# Omit a lane to detect it from project manifests; an empty list disables it.
test = ["cargo test --locked"]
build = ["cargo build --release --locked"]
after_edit = "off"        # off (default) | ask | on

[update]                  # native self-update; see getting-started.md
mode = "auto"             # auto (default) | notify | off
# pin = "0.2.0"           # hold a deliberate downgrade: newer releases are
                          # surfaced but never auto-installed

[mcp.servers.<name>]      # see docs/mcp.md for the full field reference
transport = "stdio"       # or "http"
command = "npx"           # stdio: command/args/env/cwd
# url = "https://…"       # http: url/headers/oauth_client_id/oauth_scopes
enabled = true
allow_tools = []          # empty = every discovered tool
capabilities = []         # read/write/network/shell/irreversible/untrusted_output
batching = "serialized"   # or "path_scoped"
timeout_secs = 30

[[hooks]]                 # see docs/hooks.md for the full field reference
name = "cargo-fmt"
event = "PostFileWrite"   # SessionStart/SessionEnd/Pre|PostToolUse/Pre|PostFileWrite/Pre|PostBash
matcher = { path = "**/*.rs" }          # and/or { tool = "<glob>" }
action = { type = "shell", command = "cargo fmt -- \"$BONSAI_FILE_PATH\"" }
timeout_secs = 30
blocking = false
on_failure = "warn"       # or "block" (fail-closed, blocking pre-events only)
enabled = true
```

### Verification

`/test` and `/build` run the configured (or manifest-detected) command
profiles through the agent's normal bash permission, sandbox, hook,
cancellation, and transcript-evidence path. Cargo, Node package scripts, Go
modules, and Python projects are detected from their manifests in stable
order; a `Cargo.lock` adds `--locked`. `after_edit = "on"` runs the test
profile once after a coding turn that changed workspace state (falling back
to build when no tests exist); `"ask"` prompts in the TUI and skips on
noninteractive surfaces. In headless mode, `bonsai -p /test` or
`-p /build` expands to the same bounded workflow, and stale, failed,
interrupted, or incomplete verification exits non-zero.

### Verification evidence and freshness

Bonsai captures a canonical workspace binding immediately before each
recognized verification Bash command executes. The deterministic BLAKE3
identity includes canonical repository/worktree roots, Git `HEAD`, index and
tracked-worktree state, relevant non-ignored untracked source/config inputs,
the command working directory, normalized command and verification
configuration, selected environment inputs (including `PATH`), and the
resolved executable's content fingerprint. Capture never launches the
candidate command or a `--version` probe. In a non-Git project the same model
uses a content fingerprint of relevant project inputs. Ignored build products
and unrelated untracked files are excluded from the input set; any observed
tracked-file mutation is conservatively invalidating. A mutation attributed to
the active agent task is also treated as relevant immediately, even when its
new path is not tracked yet; unrelated untracked files that appear externally
remain governed by the narrower input policy above.

Manual recognized Bash checks (including auto-backgrounded checks) and
`/test`/`/build` workflows use the same evidence path. A logical check persists
its binding, exit state, tool-call id, attempt count and timestamps,
failure-signature history, and typed terminal reason. Identical retries
aggregate into that record. If unchanged inputs
already produced the same deterministic failure, Bonsai records a
`repeated_deterministic_failure` blocked request instead of launching Bash
again. A failure is confirmed deterministic only after two executions produce
the same normalized failure signature against the same binding; changing the
workspace, command, working directory, configuration, environment, or
toolchain permits another execution.

At the true TUI/headless completion boundary Bonsai captures a delivery
binding for every check. A pass is **fresh** only when every execution binding
is valid and equals its corresponding delivery binding. Equal `HEAD` alone is
not sufficient: dirty tracked files, staged changes, relevant untracked
inputs, dependency/config changes, and toolchain/environment changes all
participate. Completion evidence is structured as `fresh`, `stale`, `skipped`,
or `blocked` and includes a short binding id. Skips and capture failures use
typed reasons such as `policy_disabled`, `user_skipped`, `cancelled`,
`environment_blocked`, and `interrupted`; freshness is never inferred from
the assistant's final prose.

These records describe evidence; they do not independently define the final
task-success policy. Cross-session execution sharing is also outside this
mechanism.

## `/config` commands

- **`/config`** — the merged view: layer paths and provenance, every MCP
  server and hook annotated with the layer that won
  (`global`/`project`/`env`) and its file, the sandbox and verification
  summary, plus any load diagnostics.
- **`/config validate`** — re-parses both layers from disk without
  restarting and prints diagnostics, so you can fix a typo and re-check
  without relaunching. (Configuration changes still take effect on
  relaunch.)
- **`/config edit [project|global]`** — prints the file path(s) to open.

## Resource directories

Beyond `config.toml`, bonsai discovers file-based resources from
conventional directories, highest precedence first:

1. `<project>/.bonsai/<kind>/`
2. `<project>/.claude/<kind>/` (cross-tool compatibility)
3. `<project>/.agents/<kind>/` (cross-tool compatibility)
4. `$BONSAI_HOME/<kind>/` (global)

where `<kind>` is `agents/` (flat `<name>.md` — [custom agents](agents.md)),
`skills/` (`<name>/SKILL.md` — [skills](skills.md)), `themes/` (flat
`<name>.toml` — [theming](theming.md)), and `memory/`
([memory](memory.md), `.bonsai` + `$BONSAI_HOME` only). A project resource
shadows a global (and built-in) one of the same name; within a project,
`.bonsai` > `.claude` > `.agents`. A `.disabled` file (one name per line) in
any resource directory suppresses entries — this is how built-in skills are
disabled per project. Project resources require workspace trust.

Provider and model catalog overrides live in `$BONSAI_HOME/providers|models`
and `.bonsai/providers|models` — see [Providers](providers.md) and
[Models](models.md).

## Environment variables

The complete environment surface, grouped. Per-provider credential/model/URL
variables are listed in [Providers](providers.md#built-in-connections).

| Variable | Purpose |
| --- | --- |
| `BONSAI_HOME` | Home directory (default `~/.bonsai`) |
| `BONSAI_CONFIG` | Replace the project config path |
| `BONSAI_DOTENV` | `0/false/off/no` disables `.env` loading |
| `BONSAI_PROVIDER` | Provider for a new headless run |
| `BONSAI_AUTONOMY` | Startup autonomy level |
| `BONSAI_SELF_REVIEW` | `auto\|on\|ask\|off` startup self-review mode |
| `BONSAI_EPISODES` | `0/false/off/no/disabled` disables task episodes |
| `BONSAI_SANDBOX`, `BONSAI_SANDBOX_NETWORK`, `BONSAI_SANDBOX_WRITABLE_ROOTS` | [Sandbox](sandbox.md) posture |
| `BONSAI_MAX_GENERATION_SECONDS`, `BONSAI_MAX_STREAMED_CHARS` | Per-attempt generation caps |
| `BRAVE_SEARCH_API_KEY`/`BRAVE_API_KEY`, `TAVILY_API_KEY`, `BONSAI_SEARXNG_URL`, `BONSAI_WEB_SEARCH_PROVIDER` | [Web search](language-intelligence.md#web-research) |
| `BONSAI_RUST_ANALYZER`, `BONSAI_TYPESCRIPT_LANGUAGE_SERVER`, `BONSAI_PYRIGHT_LANGSERVER`, `BONSAI_GOPLS` | [Language-server](language-intelligence.md) command overrides |
| `BONSAI_MEMORY_EMBEDDINGS` | `off` keeps [memory](memory.md) retrieval BM25-only |
| `BONSAI_MODELS_DEV_URL` / `_PATH` / `_TTL_SECS`, `BONSAI_DISABLE_MODELS_FETCH` | [Model catalog](models.md#modelsdev) source |
| `BONSAI_CODEX_CLIENT_VERSION`, `BONSAI_CODEX_REASONING_PERSIST` | Codex transport tuning |
| `BONSAI_COLOR_MODE`, `NO_COLOR` | [Terminal color](theming.md#terminal-color-modes) |
| `BONSAI_LOG` / `RUST_LOG` | Tracing verbosity (defaults `warn`) |
| `BONSAI_TRANSCRIPT_LOG` | Write every request/response to disk (debugging; large) |
| `BONSAI_SYMBOL_RESPECT_GITIGNORE`, `BONSAI_REPO_MAP_RESPECT_GITIGNORE` | Symbol/repo-map gitignore handling |

## `/settings`

`/settings` covers the main runtime preferences without file editing: model,
autonomy, self-review, context (smol), default credential store, appearance
(serenity, theme), per-run budgets (max turns, run time, generation, output,
tool time), session budgets (turns, output, time, cost), and sandbox
(confinement, network). Every budget is off by default; choosing a preset
opts in, and command-line limits still override saved values for that run.
Preferences persist in the local database, not in `config.toml`.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Load, layering, diagnostics | `src/config/mod.rs` |
| Schema types | `src/config/schema.rs` |
| Merge rules | `src/config/merge.rs` |
| Validation | `src/config/validate.rs` |
| `/config` command | `src/commands/config_cmd.rs` |
| Resource discovery | `src/resource/discovery.rs` |
