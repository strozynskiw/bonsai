# Development

How to build, test, and extend bonsai. [`AGENTS.md`](../AGENTS.md) is the
working guide for coding agents in this repository (style, commit
conventions, project-specific patterns) — this page is the human-oriented
companion and does not repeat all of it.

## Build, test, lint

Stable Rust, edition 2024, no toolchain pin:

```sh
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```

CI (`.github/workflows/rust.yml`) runs the same set; the release build must
succeed under `RUSTFLAGS=-D warnings`. The pre-commit hook below enforces
the fmt+clippy half locally on every commit.
`unsafe_code` is denied crate-wide (scoped, documented waivers only);
`dbg!` is a hard clippy error.

Enable the pre-commit hook once per clone — it runs rustfmt on staged files
and clippy before every commit:

```sh
git config core.hooksPath .githooks
```

Tests are inline `#[cfg(test)]` modules next to the code (the top-level
`tests/` directory is intentionally empty). Note the crate is binary-only,
so test filters run as `cargo test <filter>`. Use
`crate::tool::test_utils::TestFixture` for tool tests — it provides a temp
project root, a drained interaction channel, and a `ReadTracker`.

Other test layers:

- **Eval harness** — `bonsai eval --mode mock --json` runs the deterministic
  release-gating suites through the real agent path; see [Eval](eval.md).
- **e2e/** — tmux-driven smoke tests of the real TUI binary
  (`e2e/run.sh`), no network and no real keys; they catch wiring bugs unit
  tests can't.

## Adding a tool

1. New file `src/tool/<name>.rs` implementing the `Tool` trait: `name`,
   `description`, `parameters_schema` (use the builders in
   `src/tool/schema.rs`), a mandatory `effect_policy`, and `execute`.
2. Add its descriptor to the profile table (`src/tool/profile.rs`) and
   register it in `src/bootstrap.rs` against the right registries —
   coding, planning, smol, and/or the subagent grant set. Wire order is
   byte-stable; append thoughtfully.
3. Classify it for parallel batching in `tool_call_accesses`
   (`src/agent/batching.rs`): `ReadPath`, `WritePath`, `GlobalWrite`, or
   `Independent`. **Add a unit test for the classification** — the batching
   tests are the spec.
4. Use `ToolOutput::Edit` for anything that mutates a file (the TUI shows a
   diff card), and `ToolOutput::untrusted_context` for any external
   content.
5. If it mutates the workspace, go through `FileMutationContext` /
   `ActionPlan` so read-before-write, hooks, locks, and authorization apply.

## Adding a provider

The common case is **config, not code** — see
[Providers](providers.md#custom-connections-and-endpoints):

1. **Existing transport** (`openai-chat` / `anthropic-messages` /
   `codex-responses`): add a `[[connections]]` block to
   `models/builtin/connections.toml` (id, display name, auth kind,
   transport, base URL, env var names, default model, endpoint path, token
   counter). The catalog picks it up; models/pricing come from models.dev;
   `/authorize` and `/model` work automatically.
2. **New wire transport** (a genuinely different wire format): add a
   `Protocol` variant (`src/provider/metadata.rs`), a `ProviderFactory` +
   `Provider` in `src/provider/<id>.rs`, wire the dispatch arms in
   `CatalogProviderFactory` (`src/provider/registry.rs`), and add the
   factory to `default_registry()`. Reuse `provider/sse.rs` — per-provider
   SSE parsers are forbidden. The `Provider` trait takes a
   `CancellationToken`; honor it.
3. Authorization goes through `ProviderFactory::authorize(AuthInput)`;
   `session/` needs no edits — env-var names come from the connection spec.

Vendor wire quirks belong on the connection entry (`prompt_cache`,
`reasoning_content_echo`, `usage_frame_ends_stream`, …) with a comment
naming the failure they accommodate — capabilities set anywhere else are
inert.

Keep `models/builtin/*.toml` aligned with models.dev and live provider
listings (`scripts/update-model-catalog.sh` helps); startup logs warn on
drift unless a target is `pinned`.

## Extending other surfaces

| Surface | Recipe |
| --- | --- |
| Slash command | add metadata to `src/commands/metadata.rs` (single source of truth for name/usage/surface/busy behavior) + a handler in `src/commands/handlers/common.rs` (shared) or the TUI dispatch |
| Skill / custom agent / theme | drop files in the [resource directories](configuration.md#resource-directories) — no source changes |
| Hook / MCP server | config only — see [Hooks](hooks.md), [MCP](mcp.md) |
| Eval task | fixture + `[[tasks]]` + graders — see [Eval](eval.md#adding-a-task) |

Extensibility work must declare its boundary: namespace, trust boundary,
timeout, permission needs, and batching/access policy. Unknown external
tools default to serialized execution and explicit permission gates.

## Invariants to respect

- **Untrusted content stays data** — external content goes through
  `ToolOutput::untrusted_context`; never invent a parallel framing path.
  The regression test is `src/agent/tests/web_injection.rs`.
- **Tools never print** — return `ToolOutput`; the sink renders.
- **Secrets never enter SQLite** — store references, redact at boundaries.
- **Byte-stable prompt prefixes** — registries, indices, and the repo map
  are deliberately deterministic; changes that reorder them break provider
  caches. Model-quirk accommodations carry comments naming the failure
  pattern; don't "simplify" them away.
- **Errors keep their detail** — the malformed-tool-arguments path (tool
  name + required fields + bad payload + parse error) is the bar.
- Don't hand-edit `Cargo.lock` (CI is `--locked`); don't add async runtimes
  beyond tokio; don't add a `lib.rs`.

## Releasing

```sh
scripts/release.sh 0.2.0-rc.1 --no-push
```

The script validates a clean tree, bumps `Cargo.toml` + public install
references, runs the release checks, commits
`chore(release): prepare v<version>`, and creates the annotated tag; the
pushed tag triggers the `Release` workflow (four-target builds, checksums,
signed manifest, SBOM, attestations). Signing requires the
`BONSAI_RELEASE_PUBLIC_KEY` variable and `BONSAI_RELEASE_PRIVATE_KEY`
secret; the workflow refuses to publish on a mismatch. See
[`RELEASING.md`](../RELEASING.md) for the full runbook and
[Security model](security.md#release-integrity) for verification.

## Documentation map

These docs live in `docs/` and are split by topic — start at the
[index](README.md). Keep `README.md` (quick tour) and `AGENTS.md` (agent
conventions) in their existing roles. GitHub milestones and issues track
unfinished work, while GitHub Releases contain release notes.
