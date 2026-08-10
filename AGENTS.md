# AGENTS.md

Guidance for coding agents working in the `bonsai` repository. Read this before
touching anything; the project has conventions that are not obvious from the
code alone.

## What bonsai is

A Rust TUI coding agent — direct competitor to Claude Code / Codex CLI. Single
static binary, switchable mid-conversation across fourteen built-in connections
(OpenCode Go, OpenCode Zen, OpenAI API, Codex, Anthropic, MiniMax API, MiniMax
Coding Plan, Z.AI API, Z.AI Coding Plan, Moonshot AI API, Kimi Coding Plan,
OpenRouter, `openai-compatible`, `anthropic-compatible`). These are **not**
fourteen hand-written backends: each is a
declarative entry in `models/builtin/connections.toml` bound to one of three wire
transports (`openai-chat`, `anthropic-messages`, `codex-responses`), with model
definitions (context windows, pricing, reasoning support) sourced from
**models.dev** (`https://models.dev/api.json`, cached). The long-term goal is breadth:
adding a provider should be a config entry, not a code change. `README.md` and
`docs/overview.md` describe the product; GitHub milestones and issues are the
source for unfinished delivery work. Check the relevant milestone before
proposing work.

## Build, test, lint

Toolchain: stable Rust, edition 2024. There is no `rust-toolchain` pin.

```sh
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```

CI (`.github/workflows/rust.yml`) runs the same set. The release build must
succeed under `RUSTFLAGS=-D warnings`; no warnings, ever. There is no
`#![deny(...)]` at the crate root — denials come from clippy `-D warnings`
plus the standard toolchain lints.

## Layout

```
src/
  main.rs               thin entry point; bootstrap.rs does the real wiring
  bootstrap.rs          wires Agent + TUI, builds registries + model catalog
  cli.rs                arg parsing, subcommands (eval, headless, serve-to-be)
  agent/                agent loop, parallel tool batching, retry, compaction
  session.rs            on-disk provider/session state, env-var normalization
  model_catalog.rs      models.dev catalog + connections.toml loader, pricing
  permissions.rs        bash allow/ask/deny rules
  interaction.rs        UI-bound prompts (permission asks, question tool)
  todo.rs               TodoWrite backing store
  plan.rs               plan-canvas data model (sections + tasks)
  context.rs            system-prompt project-context block
  context_view.rs       /ctx report builder (persona + token breakdown)
  mention.rs            @-mention file/path resolution in the composer
  review.rs             /review diff-context selection (Uncommitted/LastCommit/…)
  diff.rs               unified diff model for the TUI
  copy.rs               clipboard / selection helpers
  output.rs             SharedSink trait — TUI event channel abstraction
  yolo.rs               auto-accept (/yolo) policy
  commands/             slash-command dispatch (TUI + headless handlers)
  storage/              SQLite persistence, migrations, snapshots
  background.rs         background bash tasks + `tasks` tool
  terminal.rs           process-local PTY lifecycle + bounded/redacted output
  headless.rs           `bonsai -p` print mode
  eval/                 eval runner (suite, graders, mock provider, report)
  provider/             provider trait, metadata, SSE, transforms; three wire
                        transports + the catalog-driven ProviderRegistry
  tool/                 one file per tool + mod.rs (Tool trait, ToolRegistry);
                        shared search/file_mutation/schema helpers
  tui/                  ratatui app: run/, app/ (reducer), view, keymap, theme,
                        widgets (input, modal, transcript, sidebar, plan, syntax)
  symbol/               tree-sitter extractors (Rust, TypeScript/JS, Python, Go)
  util/                 small shared helpers
models/                 builtin/connections.toml + example connector/model TOML
tests/                  integration tests (currently empty)
```

Several modules that were single files are now directories (`agent/`,
`commands/`, `storage/`, `tui/run/`, `tui/app/`) after the June 2026 refactor
wave. The legacy `config.rs` is gone (config now lives in
`model_catalog`/`session`).

`tests/` is empty — all tests are inline `#[cfg(test)]` modules next to the
code they cover. When adding a test, follow that pattern unless the test
genuinely needs to live in a separate crate.

## Adding a tool

1. Add a new file under `src/tool/<name>.rs`. The `Tool` trait is small: `name`,
   `description`, `parameters_schema` (returns `serde_json::Value`), and
   `async fn execute(args) -> Result<ToolOutput>`.
2. Register it in `bootstrap.rs` against the appropriate registry — `tool_registry`
   for the coding agent, `planning_registry` for the plan agent (read-only
   plus the plan-canvas tools).
3. If the tool touches the filesystem, classify it in
   `tool_call_accesses` (`src/agent/batching.rs`) so parallel batching works
   correctly: `ReadPath` for read-only paths, `WritePath` for writes,
   `GlobalWrite` for shell/state-mutating calls, `Independent` for anything
   else. Conflicting calls are serialized; non-conflicting calls run
   concurrently via `join_all`. **Add a unit test for any new tool's
   batching classification.**
4. Use `crate::tool::test_utils::TestFixture` for unit tests — it gives you a
   temp project root, drained interaction channel, and a `ReadTracker`.

`ToolOutput` has three variants: `Text`, `Edit { summary, diff }`, and
`Image { mime_type, base64_data, description }`. Use `Edit` whenever a tool
mutates a file so the TUI can show a diff card.

## Adding a provider

The common case is **config, not code**. `ProviderRegistry::from_catalog`
(`src/provider/registry.rs`) builds one `CatalogProviderFactory` per
`[[connections]]` entry in `models/builtin/connections.toml`; the factory derives
its `ProviderMetadata` from the connection spec and dispatches by `transport`
(`Protocol`) for streaming/model-listing and by `auth` for authorization.

1. **Existing transport (`openai-chat` / `anthropic-messages` / `codex-responses`)
   — no Rust.** Add a `[[connections]]` block: `id`, `display_name`, `auth`
   (`api-key` / `optional-api-key` / `codex-cache`), `transport`,
   `default_base_url`, the `*_env` var names, `default_model`,
   `default_endpoint_path`, `default_token_counter`. The catalog picks it up;
   models/pricing/context windows come from models.dev. `/authorize` and
   `/model` work automatically. This is how a new vendor (e.g. another
   OpenAI-compatible service) should be added.
2. **New wire transport** — only when a vendor needs a genuinely different wire
   format (e.g. Gemini). Add a `Protocol` variant in `src/provider/metadata.rs`,
   a hand-written `ProviderFactory` + `Provider` in `src/provider/<id>.rs`, wire
   the dispatch arms in `CatalogProviderFactory` (`registry.rs`), and add the
   factory to `default_registry()` so catalog connections can delegate to it by id.
3. The `Provider` trait is `Send + Sync` and gives you a `CancellationToken`
   so the TUI can interrupt the stream. Reuse the shared SSE helpers in
   `provider/sse.rs`; do not write per-provider SSE parsers.
4. Authorization goes through `ProviderFactory::authorize(AuthInput) -> AuthorizeOutcome`,
   which is what `/authorize` calls. `AuthInput::FromEnv` for
   `OPENCODE_API_KEY` / `ANTHROPIC_API_KEY` / `MINIMAX_CODING_PLAN_API_KEY`-style
   flows; `AuthInput::OpenAiCompatible { base_url, api_key, model }` for
   compatible connectors; `AuthInput::FromCodexCache` to import the Codex CLI
   session.
5. `session.rs` needs no edits — it reads env-var names from the metadata, which
   the catalog factory derives from the connection spec.

## Style

The agent style rules in `Agent::system_prompt(Coding)` apply to the human
authors too, not just the model:

- **Direct and brief.** No preamble, no restating the request, no filler.
- **Don't explain what you are about to do — do it, then state the result.**
- **Short summaries** in commit messages, PR descriptions, and the final
  user-facing message (2-3 sentences max).
- **Use the tools** — read code before changing it; use todowrite to track
  multi-step work with exactly one `in_progress` item.
- **TUI picker rows use arrow-only selection.** Use a leading ASCII `>` marker
  for the selected row in popups/lists; don't add reversed-row or background
  highlighting unless explicitly requested. When picker rows include details
  or descriptions, keep those in a separate aligned column instead of flowing
  them directly after the primary label. In authorization pickers, authorized
  provider names are green and unauthorized provider names use the normal text
  color; keep auth labels in the legend only and don't show a separate
  current-provider marker unless requested.

Rust-specific style (from `.agents/skills/rust-code-writer/SKILL.md`, which
agents must follow):

- No `unwrap()`/`expect()` in library code; `Result` + `?` for recoverable
  errors, `thiserror` enums at module boundaries, `anyhow` at the
  application top level (`main.rs`).
- Newtypes for semantic distinction; enums instead of boolean flags; builder
  pattern for non-trivial construction.
- Accept the most flexible type in arguments: `&str` over `String`, `&[T]`
  over `Vec<T>`, `&Path` over `PathBuf`.
- Eagerly derive `Debug` on every public type; document public APIs with
  `///` and include `# Errors` / `# Panics` / `# Safety` sections where
  applicable.
- `unsafe` is forbidden unless absolutely necessary; if used, encapsulate
  it in a small module with a `# Safety` section.
- No `clone()` to silence the borrow checker; redesign the data flow.

## Committing

Conventional Commits, going forward: `type(scope): summary`. The history is
mixed — older commits use bare prose (`Harden agent execution…`) or
`module: summary` (`agent: tighten…`) — but **new commits use the typed form**.
`feat`/`fix` are the common cases; the existing typed history is the bar
(`feat(provider): activate Anthropic prompt caching`,
`fix(tui): copy mouse selection on release`).

- **Types:** `feat`, `fix`, `refactor`, `perf`, `docs`, `test`, `chore`,
  `build`, `ci`. Pick the one that names the *intent* of the change, not the
  files it touched. Bare `fmt` becomes `chore(fmt):` or folds into the commit
  that caused it.
- **Scope** is the module or area, lowercase, drawn from the layout above:
  `provider`, `agent`, `tui`, `compaction`, `eval`, `cli`, `model-catalog`,
  `safety`, `storage`, `roadmap`, … Encouraged but optional; omit it only for
  genuinely cross-cutting changes.
- **Summary** is imperative mood, lowercase, no trailing period, ≤ ~70 chars
  ("activate Anthropic prompt caching", not "Activated…" or "activates…").
- **One logical change per commit.** Don't mix a refactor with a behavior
  change; keep pure formatting churn out of feature commits.
- **Body explains *why*, not *what*** — the diff already shows what. Blank line
  after the subject, wrap at ~72 cols. Reference the relevant plan or GitHub
  issue when the change implements one.
- **Green tree before you commit.** `cargo fmt`, `cargo clippy --all-targets
  --all-features -- -D warnings`, and `cargo test --locked` must pass — the
  subject claims the change works, so it has to.
- **Breaking changes** get a `!` before the colon (`feat(provider)!: …`) plus a
  one-line note in the body.
- Commit only when asked; if the work touched `master`, branch first.

## Project-specific patterns

These are the things in this codebase that surprised me, captured so the
next agent doesn't have to relearn them:

- **Two `ToolRegistry`s.** `bootstrap.rs` builds a coding registry (full tool
  set) and a planning registry (read-only tools + plan canvas tools).
  `Agent::set_mode` swaps which one is active; the conversation history
  is preserved across the switch. When adding a tool, decide which
  registry it belongs in.
- **Tool batching lives in the agent, not the registry.** `tool_call_batches`
  classifies each call (`read`/`glob`/`grep`/`symbol_search` → read paths,
  `write`/`edit` → write paths, `bash`/`question`/plan tools → global write)
  and groups non-conflicting calls into a single `join_all`. Path
  conflicts are detected by prefix overlap, not exact match. The unit
  tests in `src/agent/batching.rs` (and `src/agent/tests/`) are the spec —
  add to them when you change the rules.
- **Extensibility work declares its boundary.** MCP servers, hooks, skills,
  custom slash commands, and external tools must declare namespace, trust
  boundary, timeout, permission needs, and batching/access policy. Unknown
  external tools default to serialized execution and explicit permission gates
  until a narrower access policy is proven and tested.
- **Untrusted content stays data.** Web/MCP/file-derived content is untrusted
  and must not become executable instruction without explicit user, config, or
  permission gating. Tool calls triggered by untrusted output re-enter the same
  permission path as any other external action. The canonical mechanism is
  `ToolOutput::UntrustedContext` (built via `ToolOutput::untrusted_context` /
  `wrap_untrusted_content` in `src/tool/mod.rs`): it wraps the content in a
  self-describing, delimiter-escaped frame that the model sees as data, and —
  unlike `TrustedContext` — is **never** promoted to a system message. WebFetch
  (M5.5) uses it; MCP tool results (M5.2) must reuse it rather than inventing a
  parallel path. The regression test is `src/agent/tests/web_injection.rs`.
- **`SharedSink` is the abstraction TUI uses to receive events.** The
  `Output` interface (`sink.thinking`, `sink.tool_started`,
  `sink.tool_finished`, `sink.tool_finished_with_diff`,
  `sink.status`) is how the agent reports progress without knowing
  about ratatui. Test code uses `StdoutSink`; production wires a TUI sink
  in `tui/run/`.
- **Provider errors are typed.** `ProviderError::Http` carries the status,
  message, and parsed `retry-after`. `is_retryable()` covers 429/5xx;
  `chat_stream_with_retry` honors the header and caps at
  `MAX_PROVIDER_RETRIES` (3). Cancellation during the backoff sleep
  short-circuits to `StreamedResponse::interrupted()`.
- **Context compaction is char/4 + drop-old.** The `char/4` heuristic in
  `estimate_text_tokens` is the budget check; compaction drops everything
  between the system message and the last 20 messages, replacing it with
  a single system message that says "X messages were omitted". Counting is
  provider-*family* estimation today; exact local tokenizers remain future work.
- **The `ReadTracker` enforces read-before-edit.** `WriteTool` and `EditTool`
  reject paths the model hasn't read this session, with an mtime staleness
  check. If a tool writes to disk, it should respect the same boundary
  in tests (`TestFixture` builds the tracker for you).
- **The system prompt is constructed once per mode.** `Agent::system_prompt`
  returns the static persona; `system_message` appends the
  `context::project_context(cwd)` block (cwd, git state, steering files)
  at runtime. Tests pass an empty `system_context` string to keep the
  system message deterministic.
- **Episodes are default-on; `BONSAI_EPISODES=0` is the kill switch.** The
  agent partitions the parent lane into task-scoped episodes (`src/episode.rs`,
  `src/agent/episodes/`): `set_session_title` changes and hard resets close
  them, closed episodes evict wholesale into `[Episode archived]` card markers
  behind the rewrite-economics guard, and the `recall` tool pages the archived
  bytes back inside an untrusted frame. Explicit opt-out means zero tracking,
  no recall registration, byte-identical disabled tool arrays, and no episode
  persistence. The invariants are covered by `src/episode.rs` and
  `src/agent/tests/episodes.rs`.

## Environment & secrets

- `.env` is loaded via `dotenvy`. `.env.example` documents the required
  variables (`OPENCODE_API_KEY`, plus optional per-provider keys).
- `BONSAI_LOG` / `RUST_LOG` set tracing verbosity (defaults to `warn` for
  `bonsai`, `info` only when one is set).
- `BONSAI_TRANSCRIPT_LOG=<path>` writes every request/response to disk
  (debugging only — large).
- Secrets come from env, never from disk. Don't add a secrets file to the
  repo, and don't log API keys even at `trace` level.

## What not to do

- Don't add a `lib.rs` exposing the internals. This is a binary crate on
  purpose.
- Don't add async runtimes other than tokio. The whole codebase is
  `#[tokio::main]`.
- Don't bypass the `Output` sink to print to stdout/stderr from inside
  tools. Tools return `ToolOutput`; the TUI decides how to render.
- Don't add new provider SSE parsers. Reuse `provider/sse.rs`.
- Don't ship a `Result` error that swallows the parse detail. The
  malformed-arguments test in `src/agent/` is the bar: the model gets
  tool name, required fields, the bad payload, and the parse error.
- Don't add a `MIGRATION.md`, `CHANGELOG.md`, or similar without asking.
  Release notes live in GitHub Releases.
- Don't modify `Cargo.lock` by hand. The CI build is `--locked`; if a
  dep needs updating, run `cargo update -p <crate>` and commit the diff.

## References

- GitHub milestones and issues — current delivery plan and exit bars.
- `.agents/skills/rust-code-writer/SKILL.md` — full Rust style guide.
- `models/builtin/connections.toml` — declarative provider registry; the place
  to add a provider on an existing transport.
- `plans/external_benchmark_adapters.plan` — external benchmark adapter design.
- `agent/` tests — spec for tool batching, error messages, retry, compaction.
