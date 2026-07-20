---
name: customize
description: Add, edit, troubleshoot, or explain any bonsai customization — custom agents, subagents and personas, themes, skills, MCP servers, hooks, verification/sandbox/read-isolation config, memory, steering files, providers, models, and local endpoints. Load before changing bonsai extension files or diagnosing a customization error. For providers, models, API keys, or local endpoints, also load provider-setup and use it as the schema authority.
---

# Customize bonsai

Implement the requested customization; do not stop at describing a sample when
the target workspace is writable. Keep the change small, preserve existing
entries, and use the exact on-disk schema below.

## Definition of done

1. Identify the extension surface and whether it belongs to this project or the
   user's global bonsai home. Read an existing target file before editing it.
2. Prefer the native scaffold or the smallest inherited example. Change only
   values needed for the request; never invent a plausible field name.
3. Validate through the matching bonsai command. A file being present, looking
   complete, or parsing as generic TOML/YAML is not proof that bonsai accepts it.
4. If the command is TUI-only and cannot be run from the current tool surface,
   give the user the exact command and report validation as pending. Never say
   "correct", "complete", or "confirmed" while the latest observed output is an
   error or before a later successful load has superseded that error.
5. Treat an `unknown field ... expected one of ...` diagnostic as the
   authoritative whitelist: remove or translate the unknown field, then validate
   again. Fix every reported problem, not only the first one.

Use ASCII `"` quotes in TOML/YAML examples. Do not turn display typography such
as smart quotes into file syntax.

| Request | Fast path | Direct file | Validate / activate |
| --- | --- | --- | --- |
| Provider, model, key, local endpoint | load `provider-setup`; `/providers add` is the TUI wizard | `$BONSAI_HOME/providers/*.toml` + `models/*.toml` | restart, `/providers list`, `/model` |
| Agent, persona, or subagent | `/agents new` is the TUI composer | `.bonsai/agents/<name>.md` | restart after hand-edit, then `/agents` |
| Theme | `/theme export <name>` creates a known-good palette | `.bonsai/themes/<name>.toml` | `/theme <name>` (rescans live) |
| Skill | copy the minimal definition below | `.bonsai/skills/<name>/SKILL.md` | restart, `/skills`, `/skill <name>` |
| MCP server | edit config | `[mcp.servers.<name>]` in `config.toml` | `/config validate`, restart, `/mcp` |
| Lifecycle hook | edit config | `[[hooks]]` in `config.toml` | `/config validate`, restart, `/hooks test <name>` |
| Tests, builds, sandbox, read isolation | edit config | `.bonsai/config.toml` | `/config validate`, restart |
| Durable fact | `/remember <fact>` | `.bonsai/memory/` or global memory | `/memory` |
| Project instructions | `/init` creates a starter | root `AGENTS.md` | next launch |

Slash commands are user-facing TUI actions, not shell commands. Do not try to
execute `/theme`, `/agents`, or another TUI command through bash.

## Scope and precedence

Default to project scope for repository-specific behavior and global scope for
the user's reusable personal setup. `BONSAI_HOME` defaults to `~/.bonsai`.

Resource directories (`skills/`, `agents/`, `themes/`) resolve highest-first:
project `.bonsai/` > `.claude/` > `.agents/` > global `$BONSAI_HOME/` >
built-in. A higher definition with the same name shadows a lower one. A
`.disabled` file disables listed skill/theme names (one per line; `#`
comments); park a custom agent with `enabled: false` in its frontmatter.

Project skills, agents, config, hooks, MCP servers, and steering instructions
are inert in an untrusted workspace. `config.toml` layers project
`.bonsai/config.toml` over global `$BONSAI_HOME/config.toml`; env vars win over
both. Never place secrets in repository files.

## Themes: exact schema, no aliases

Prefer an inherited theme. It is both faster and safer because only requested
overrides are needed:

```toml bonsai:theme-file
# .bonsai/themes/trollo.toml
extends = "gruvbox"
blurb = "warm copper tones for project demos"
bg = "#1b1712"
panel = "#28211a"
border_active = "#d79921"
assistant = "#fabd2f"
progress = "#fabd2f"
text = "#ebdbb2"
muted = "#a89984"
```

`accent` is **not** a theme field. Neither are generic aliases such as
`primary`, `foreground`, `background`, or `cursor`. Translate a requested
"accent" to the roles that should visibly use it: usually `border_active`,
`assistant`, and `progress`; use `tool`, `edit`, `success`, `agent_accent`, or
`plan_accent` only when that semantic area should share the color. Do not emit
an `accent = ...` line.

The only non-color keys are `blurb` and `extends`. The exhaustive color-key
whitelist is:

```text
bg panel panel_dark border border_active
user_block assistant_block thinking_block tool_block result_block edit_block
todo_block error_block peer_block
text muted dim
user assistant progress tool edit error todo success peer selection_bg
added added_bg removed removed_bg lineno path command
syntax_comment syntax_string syntax_number syntax_keyword syntax_type syntax_function
context_system context_user context_assistant context_tool context_tool_schema
agent_accent agent_border plan_accent plan_border
```

- Every color must be a quoted six-digit `"#rrggbb"` value.
- `extends` must name a built-in: `forest`, `ocean`, `paper`, `ember`,
  `sakura`, `glacier`, `dawn`, `catppuccin`, `catppuccin-latte`, `gruvbox`,
  `gruvbox-light`, `nord`, `tokyonight`, `solarized`, `solarized-light`,
  `dracula`, `contrast`, or `contrast-light`. A custom theme cannot be a base.
- Without `extends`, every color role above is required. Use
  `/theme export <name>` when a standalone complete palette is actually needed.
- The filename stem becomes the lowercased theme name; only `a-z`, `0-9`, `_`,
  and `-` are allowed.
- `/theme <name>` rescans theme directories without a restart. Success means it
  selects the theme without a diagnostic. If it reports an error, edit the
  named file and run the same command again before claiming completion.

## Custom agents, personas, and subagents

Use `/agents new` for the fastest interactive path. For direct authoring, write
one flat Markdown file. The definition may join the mode switcher, be delegated
as a subagent, or both:

```markdown bonsai:agent-file
---
name: test-fixer
description: Diagnoses and repairs focused test failures
enabled: true
surface: [mode, subagent]
view: todo
color: amber
tools: [read, grep, definition, references, edit, bash]
model: f
effort: low
---
You diagnose one focused test failure at a time. Read the relevant code and test,
make the smallest repair, run the narrow test, and report the changed files and
the observed result. Do not broaden the change without evidence.
```

- Required frontmatter: `name` and `description`; match `name` to the filename
  stem. Provide a non-empty prompt body. `enabled` defaults to `true`.
- Omit `tools` for the default read-only set. Exact grantable names are
  `project_info`, `read`, `read_region`, `read_symbol`, `glob`, `grep`,
  `symbol_search`, `definition`, `references`, `hover`, `workspace_symbol`,
  `git`, `write`, `edit`, `bash`, `rename_symbol`, `skill`, and `question`.
  Unknown or session-internal tools are ignored and shown by `/agents`.
- A custom agent may receive mutating tools such as `write`, `edit`, or `bash`;
  their calls still pass through the parent's permission and sandbox policy.
  Mutating subagents serialize and should not be launched in background mode.
- `model` accepts a one-letter shortcut or the same `connection:model` selector
  as `/model`, for example `ollama:qwen3-coder` or
  `codex:openai/gpt-5.5`. `effort` accepts `minimal`, `low`, `medium`, `high`,
  `xhigh`, or `max`. Omit both to inherit the parent model.
- `view` is `chat`, `todo`, or `canvas`. `color` is a supported named persona
  color or `#rrggbb`. `surface: [subagent]` makes a helper-only definition;
  omitting `surface` makes it available as both a mode and a subagent.
- A custom definition shadows a built-in of the same name. Hand-authored files
  load on restart; the TUI composer reloads its own writes immediately. Verify
  origin, enabled state, and any ignored tools with `/agents`.

## Skills

A skill is a directory containing `SKILL.md`. Put all trigger conditions in the
frontmatter description because only the name and description are advertised
until the body is loaded.

```markdown bonsai:skill-file
---
name: deploy
description: Build, verify, and ship this project. Load before any deploy, publish, or release task.
activation:
  markers: [scripts/deploy.sh]
---
# Deploy

1. Run the release build and full test suite; stop on failure.
2. Deploy staging, verify health, then deploy production.

Never deploy from a dirty working tree.
```

- Required: `name` and `description`; match the directory name. `activation`
  is optional. `markers` are root-relative files/directories and `extensions`
  are file extensions; any match activates the skill. Omit it to stay active.
- The body is capped at 16 KB. Keep core instructions concise rather than
  relying on content beyond the cap. `allowed-tools`, `model`, and `effort` are
  reserved and do not enforce behavior yet.
- New hand-authored files load at startup. Use `/skills` to inspect disposition
  and `/skill <name>` to load and test one after restart.

## MCP servers

Add servers to project or global `config.toml`:

```toml bonsai:config-file
[mcp.servers.docs]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "./docs"]
capabilities = ["read"]
allow_tools = ["read_file", "list_directory"]

[mcp.servers.context7]
transport = "http"
url = "https://mcp.context7.com/mcp"
headers = { Authorization = "Bearer ${CONTEXT7_API_KEY}" }
capabilities = ["network"]
```

- `stdio` uses `command`, optional `args`/`env`/`cwd`; `http` uses `url` and
  optional `headers`. `${VAR}` expands at connection time.
- Declare the narrowest accurate capabilities from `read`, `write`, `network`,
  `shell`, `irreversible`, and `untrusted_output`. Omitted declarations take the
  most cautious posture. `batching` is `serialized` (default) or `path_scoped`;
  `enabled = false` parks a server; `timeout_secs` defaults to 30.
- Tools appear as `mcp.<server>.<tool>` and their output remains untrusted data.
  Run `/config validate`, restart, inspect `/mcp`, and test a harmless read call
  before reporting the server operational.

## Hooks

```toml bonsai:config-file
[[hooks]]
name = "cargo-fmt"
event = "PostFileWrite"
matcher = { path = "**/*.rs" }
action = { type = "shell", command = "cargo fmt -- \"$BONSAI_FILE_PATH\"" }
timeout_secs = 20

[[hooks]]
name = "protect-dotenv"
event = "PreFileWrite"
matcher = { path = "**/.env*" }
blocking = true
on_failure = "block"
action = { type = "shell", command = "echo 'writes to .env are blocked' >&2; exit 2" }
```

- Exact events: `SessionStart`, `SessionEnd`, `PreToolUse`, `PostToolUse`,
  `PreFileWrite`, `PostFileWrite`, `PreBash`, `PostBash`.
- Actions are `{ type = "shell", command }`, `{ type = "http", url }`, or
  `{ type = "llm_prompt", prompt }`. LLM hooks must return strict JSON with
  `decision` (`allow` or `block`) and `reason`.
- `blocking` applies only to `Pre*` events. `on_failure` is `warn` (default) or
  `block`. Project shell/HTTP hooks require one-time user approval.
- Run `/config validate`, restart, then `/hooks test <name>`. A successful
  synthetic result, not merely presence in `/hooks`, is the completion proof.

## Core config

```toml bonsai:config-file
schema_version = 1

[verification]
test = ["cargo test --locked"]
build = ["cargo build --locked"]
after_edit = "off"

[sandbox]
writable_roots = ["/opt/shared-cache"]
deny_network = false
```

- Verification lists are ordered. An empty list disables the lane; omission
  enables manifest auto-detection. `after_edit` is `off`, `ask`, or `on`.
- Workspace reads are optimistic. Mutations serialize across Bonsai sessions
  and reject stale file content so the agent can re-read and retry.
- Sandbox roots extend the allowlist; do not broaden them without the user's
  requested need. Config changes apply on launch. Use `/config validate` for
  parse diagnostics and `/config` for the merged view and provenance.

## Memory and steering

- `/remember <fact>` writes global memory; `/remember project <fact>` writes
  `.bonsai/memory/`. Use `/memory` to inspect it. Store durable facts, not facts
  already present in code or steering files.
- Root `AGENTS.md`, `CLAUDE.md`, and `.cursorrules` become project instructions.
  Prefer `AGENTS.md`; `/init` creates a starter.

## Providers and models

Immediately load `provider-setup` before touching providers, models, API keys,
Ollama, LM Studio, vLLM, OpenAI-compatible, or Anthropic-compatible endpoints.
Follow its connection/target split and validation sequence. Do not improvise
provider or model fields from this skill, and never store an API key in TOML.

## Do not invent unsupported extension points

- Keybindings are hardcoded; `/keys` only displays them.
- Custom slash commands are not shipped; `[commands]` is reserved.
- `[skills]` and `[providers]` in `config.toml` are reserved no-ops.
- Plugin bundles are not shipped. Use only the extension surfaces listed above.
