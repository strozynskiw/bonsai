# Skills

Skills are reusable instruction sets loaded **on demand** — the prompt
carries only a one-line index per skill; the body enters context only when
relevant. Skills are the one kind of external content bonsai injects as
*trusted* instructions, which is why project skills sit behind
[workspace trust](security.md#workspace-trust).

## Format

Each skill is a directory containing a `SKILL.md` with YAML frontmatter:

```markdown
---
name: deploy
description: Build, verify, and publish this project
---
Follow the repository release runbook. Stop before an external write unless it is
authorized by the current permission policy.
```

| Field | Required | Notes |
| --- | --- | --- |
| `name` | yes | matches the directory name |
| `description` | yes | the one-liner shown in the index and `/skills` |
| `activation` | no | `{ markers = [...], extensions = [...] }` — the skill is active if any marker file exists at the project root **or** any file with a listed extension exists in the tree; omitted = always active |
| `allowed-tools`, `model`, `effort` | recognized | **rejected** — a skill declaring them is skipped rather than silently un-enforced |

Bodies are capped at 16 KiB. Unknown frontmatter keys are ignored
(cross-tool compatibility).

## Discovery and precedence

Standard [resource directories](configuration.md#resource-directories),
highest first: `.bonsai/skills/` > `.claude/skills/` > `.agents/skills/` >
`$BONSAI_HOME/skills/`, with built-ins below all of them. A
higher-precedence skill of the same name shadows the lower one. Project
skills activate only after the workspace is trusted and bonsai restarts.

## Built-in skills

Twelve built-ins ship in the binary:

- **`customize`** and **`provider-setup`** — always active. They teach the
  agent every bonsai extension surface ("add my Ollama models", "make me a
  darker theme", "write a deploy skill"); their examples are CI-checked
  against the real schemas so they can't drift from the binary.
- **Language writers** — `rust-writer`, `typescript-writer`,
  `javascript-writer`, `python-writer`, `go-writer`, `java-writer`,
  `php-writer`, `ruby-writer`, `shell-writer`, `web-writer` — each gated on
  an activation rule (e.g. `Cargo.toml`/`*.rs` for `rust-writer`), so only
  the relevant ones ride a given project's prompt.

## How the model uses skills

The cacheable prompt prefix carries a **Skills index** (one line per active
skill, capped at 50). When a task matches, the model calls the `skill` tool
with the exact name; the body returns as trusted context. Loading is
deliberate — the index tells the model what exists; the body costs context
only when fetched.

## Commands

- **`/skills`** — the skill manager: every definition with kind, provenance
  (project/global/built-in), activation hint, loaded state, and
  shadowed/disabled status. Space toggles a built-in on/off (persisted via a
  `.disabled` file in `.bonsai/skills/`); Enter loads/unloads a body for the
  session.
- **`/skills disable|enable <built-in>`** — the same toggle from the command
  line.
- **`/skill <name>`** — load one skill's instructions into context
  explicitly.

Edits to skill files are picked up by a registry reload — mid-session
enable/disable is live.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Registry, precedence, rendering | `src/resource/skill.rs` |
| Frontmatter parsing | `src/resource/frontmatter.rs` |
| Activation rules | `src/resource/activation.rs` |
| Built-ins | `src/resource/builtin.rs`, `skills/builtin/` |
| The `skill` tool | `src/tool/skill.rs` |
