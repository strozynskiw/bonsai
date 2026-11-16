# Code audit — refactor, performance, improvements

Date: 2026-11-16 · Base: `master` @ `18f3a9f1` (v0.5.1) · Scope: all of `src/`

## Method and limits

- `cargo clippy --all-targets --all-features -- -D warnings`: **clean**.
- Metrics from `wc`, `awk` and `grep` over `src/` (≈393k lines incl. tests; tests
  are ≈40% of it). "Production LOC" = lines before the first `#[cfg(test)]`.
- Function lengths in the main findings were re-verified with brace matching
  (the first draft's `fn`-to-`fn` distance overstated two of them; corrected). I read the code of every
  finding below; I did not profile anything at runtime. Performance claims are
  by inspection and should be confirmed with a benchmark before/after.
- `unwrap()`/`expect()` and `lock().unwrap()` hits in `background_wake.rs`,
  `memory/store.rs`, `agent/retry.rs`, `provider/opencode.rs` are all inside
  test modules — **not** production issues. Production code is disciplined here.

## Summary

Overall health is good: clippy-clean, no stray `unwrap` in production paths,
`#[allow(dead_code)]` used only 5 times, one `allow(large_enum_variant)`. The
debt is concentrated in a handful of **very large functions and files** (the
agent loop, the TUI event loop, provider stream handlers) and in **34
`too_many_arguments` suppressions** that point to missing parameter structs.
The only concrete runtime-cost issue found is a synchronous whole-tree walk in
`BashTool`'s verification cache. The release profile is also unconfigured.

## Status (updated after implementation)

| # | Finding | Status |
|---|---------|--------|
| 1 | Bash verification fingerprint on async path | **Done** (`spawn_blocking`) |
| 6 | Release profile | **Done**, measured (-31% binary) |
| 3 | `execute_tool_calls` | **Partly done**: planning, scoped steering, binding release and skip-and-break extracted; result folding left (shares too much state) |
| 4 | Provider stream state | **Done for opencode** (`ChatStreamState`). A shared accumulator across transports was *not* done: Anthropic content blocks and Codex Responses events have a different shape, so it would not earn its keep |
| 9 | Re-measured | `modal_identity` dropped; `handle_runtime_action` still open |
| 11 | `Result<_, String>` | **Dropped**: ~40 sites are slash-command parsers whose `String` is the user-facing usage message; typed errors would add churn without callers that match on them |
| 2, 5, 8, 10 | event loop, arg structs, state split, big files | Open (large, need interactive TUI verification) |

Operational note: after `tests/performance_baseline.rs`, check for orphaned
`target/release/bonsai` processes; two left running at 500%+ CPU made
`server::tests::wall_time_budget_*` time out until they were killed.

## High

### 1. Synchronous whole-project walk on the async path (bash verification cache)
**File**: `src/tool/bash.rs:544-575`, called at `:412`, `:794`, `:1019`
**Category**: Performance / Concurrency · **Effort**: M

`workspace_fingerprint` runs `walkdir` over the entire project root (skipping
only `.git` and `target`), calls `std::fs::metadata` on every file and hashes
path + size + mtime. It is called up to **twice per cacheable cargo command**
(before running, and again after success to validate the key) plus once in the
background watcher, all from `async` code on a tokio worker thread. In a repo
with `node_modules`, `vendor/`, `.venv` or large asset trees this blocks a
worker for tens–hundreds of ms per verification and ignores `.gitignore`.

**After**: move the walk into `tokio::task::spawn_blocking`; use
`ignore::WalkBuilder` (already a dependency) so ignored trees are skipped; take
the "before" fingerprint once and reuse it for the cache key. Optionally cap
the walk and treat overflow as "uncacheable" (conservative, still correct).

### 2. `tui::run::event_loop::run` is a ~1,300-line function
**File**: `src/tui/run/event_loop.rs:2594-3910`
**Category**: Architecture · **Effort**: L

The prologue destructures a 23-field `TuiRuntime`, then one function owns
terminal setup, theme restore, the `select!` loop and all arms. File is 5.2k
lines and is the 2nd most-churned file (19 commits in 60 days), so every
feature touches it and merges conflict. Five of the 34 `too_many_arguments`
suppressions are in this file (`:1038`, `:2086`, `:2171`, `:2359`, `:3967`).

**After**: split into `setup` (build `LoopState` from `TuiRuntime`), one
handler per event source (`on_key`, `on_runtime_event`, `on_agent_event`,
`on_tick`) taking `&mut LoopState`, and a thin `select!` driver. The bundle
`RuntimeActionDeps` already exists in `runtime_actions.rs` — extend that
pattern instead of passing 8–10 loose arguments.

### 3. `Agent::execute_tool_calls` (~700 lines) and the 6.2k-line `run_loop.rs`
**File**: `src/agent/run_loop.rs:954-1650`, `call_model_on_current_provider` (~300 lines)
**Category**: Architecture · **Effort**: L

`run_loop.rs` is the most-churned file (23 commits/60 days) and mixes
batching, hook rewrites, budget serialization, interruption, queued messages
and result folding in one method with `&mut self` plus six parameters.
`run_loop/coordinator.rs` and `run_loop_executor.rs` show the split has begun;
finish it: pure planning step (`plan_batches(calls, registry, hooks, budget) ->
Vec<Batch>`, unit-testable without a provider), execution step, and outcome
folding. The planning part is already spec'd by `src/agent/batching.rs` tests.

### 4. Provider stream handlers duplicate the accumulation state machine
**File**: `src/provider/opencode.rs:1041` (`chat_stream_with_context`, 397 lines), `anthropic.rs:1469`
(`handle_anthropic_event`, 181 lines, too many args), `codex.rs:586` (82 lines + helpers).
Re-measured with brace matching: only the opencode method is ~400 lines.
**Category**: Duplication / Architecture · **Effort**: L

Each transport re-implements the same state machine: accumulate text /
reasoning / fragmented tool-call arguments, track usage, map terminal reason,
feed the sink. AGENTS.md says "reuse `provider/sse.rs`", but only SSE framing
is shared. Extract a `StreamAccumulator` (text, reasoning, tool-call buffers,
usage, finish reason) with `push_text`, `push_tool_fragment`, `finish()`; each
transport then only maps its wire events to those calls. The existing
fragment-reassembly tests (`chat_stream_reassembles_fragmented_*`) already pin
behavior and become the safety net. Also removes the need for
`#[allow(clippy::too_many_arguments)]` on `anthropic.rs:1468`.

## Medium

### 5. 34 × `#[allow(clippy::too_many_arguments)]`
**File**: e.g. `tui/run/event_loop.rs`, `tui/widgets/modal/common.rs:952,1011`,
`tui/widgets/transcript/message.rs:604,694`, `server/controller.rs:988`,
`provider/metadata.rs:467`, `memory/mod.rs:152`
**Category**: Type safety · **Effort**: M (mechanical, per site)

Each suppression is a missing struct. Introduce small context/options structs
(`RenderCtx`, `ModalLayout`, `StreamCtx`, `ProviderMetadataSpec`). Do these
incrementally as the surrounding files are split (findings 2–4). Good CI
ratchet: a test or script that fails if the count grows above the current 34.

### 6. `release` profile is unconfigured — DONE (measured)
**Result**: `lto=thin`, `codegen-units=1`, `strip=symbols`: 85.0 MB → 58.6 MB; startup median 2258→2266 ms (noise), idle RSS 178→173 MB, first-delta 43→38 ms. Cost: release build ≈6 min; no symbol names in release backtraces.
**File**: `Cargo.toml:25-40` (only `[profile.test]` customisations exist)
**Category**: Build / Performance · **Effort**: S

No `[profile.release]`: default `codegen-units=16`, no LTO, no `strip`, default
`panic=unwind`. The local release binary is ≈85 MB (`target/release/bonsai`;
includes tree-sitter grammars, tokenizers, sqlx, axum, rmcp). Try
`lto = "thin"`, `strip = "symbols"`, `codegen-units = 1` (dist profile only if
CI time matters). Expect a notably smaller binary and some throughput gain;
**measure** with the existing `tests/performance_baseline.rs` and check that
backtrace/panic reporting still meets the project's needs before enabling
`strip`. Do not set `panic = "abort"` without checking that cancellation /
subagent isolation does not rely on `catch_unwind`.

### 7. Heavy `.clone()` density in a few production files
**File**: `tool/bash.rs` (313 hits incl. tests), `tool/registry_assembly.rs`
(248), `tui/run/event_loop.rs` (186), `eval/run.rs` (120),
`tui/runtime_actions.rs` (113), `model_catalog/mod.rs` (91)
**Category**: Ownership · **Effort**: M

Counts include test code, so many are fixture setup — I did **not** verify
each as redundant. Highest-value candidates are `Arc`-able shared state cloned
per event in the TUI loop and catalog structures cloned for each picker
render. Recommended: profile the TUI idle/streaming frame path first (flamegraph
or `perf_baseline`) and fix only what shows up; avoid blanket clone hunting.

### 8. `AppState` (~300 lines of fields) and `Agent` (~210 lines of fields)
**File**: `src/tui/app.rs:189-492`, `src/agent/mod.rs:466-676`
**Category**: State management · **Effort**: L

Both are oversized aggregates, and the code that mutates them is split across
`app.rs` (1.7k prod LOC), `event.rs` (1.6k), `runtime_actions.rs` (3.5k). Group
fields into sub-structs by lifecycle (composer, transcript, modal stack,
sidebar, background tasks) so reducers take only the slice they need. This also
makes `apply_runtime_event` (~320 lines) and `apply_event` (~190) splittable by
slice. Do after finding 2.

### 9. `handle_runtime_action` (366 lines)
**File**: `src/tui/runtime_actions.rs:148`
**Category**: Architecture · **Effort**: M

A command dispatcher: one match arm → one function in a per-domain submodule.
*Correction:* an earlier draft also listed `modal_identity`; re-measured, it is
~25 lines (the first measure counted the distance to the next `fn`). Dropped.

### 10. Large files that still hold several concerns
**File**: `storage/peers.rs` (1.9k prod LOC), `model_catalog/mod.rs` (1.9k prod
LOC, `mod.rs` as a catch-all), `headless/mod.rs` (1.8k), `tool/mod.rs` (1.5k),
`terminal/implementation.rs` (1.4k), `agent/compaction/draft.rs` (1.5k)
**Category**: Cohesion · **Effort**: M each

`model_catalog/mod.rs` and `tool/mod.rs` should hold re-exports and the core
trait, with loaders/pricing/output types in sibling files. `storage/peers.rs`
mixes type definitions (≥9 pub types) with SQL; split `types.rs` / `queries.rs`.

## Low

### 11. `Result<_, String>` in a few boundaries
**File**: `tui/local_model_wizard.rs` (5), `config/validate.rs` (3),
`commands/mcp_cmd.rs` (3), `agent/run_loop_executor.rs` (3)
**Effort**: S — replace with `thiserror` enums at module boundaries
(AGENTS.md style rule) so callers can match instead of string-compare.

### 12. `std::sync::Mutex` for `tool_schema_cache` in an async `Agent`
**File**: `src/agent/mod.rs:589`, accessor at `:1043`
**Effort**: S — fine as long as the guard is never held across `.await`.
Confirmed only the accessor returns a guard; add a comment or clippy
`await_holding_lock` (already part of the default clippy set and passing) as the
guard. No change needed unless a future caller awaits while holding it.

### 13. Vendored `model2vec-rs`
**File**: `Cargo.toml` (`path = "vendor/model2vec-rs"`)
**Effort**: S — track the upstream issue referenced in the comment and drop
the patch when feature separation lands; vendored code is outside clippy/CI
scrutiny.

## Already tracked

`plans/` is empty and I did not cross-check GitHub milestones/issues (no
network access to them in this run). Before starting, search issues for
"event_loop", "run_loop split" and "too_many_arguments".

## Recommended order

1. **#1 verification fingerprint** (done) — small, isolated, real runtime win.
2. **#6 release profile** — one-line experiment, measure with
   `performance_baseline`; ship only if binary size/throughput improves.
3. **#4 `StreamAccumulator`** — removes the most duplicated logic and is well
   covered by existing provider tests.
4. **#3 plan/execute split of `execute_tool_calls`** — hot, high-churn path;
   pure planning function is easy to test first.
5. **#2 + #5 + #9** together: split the TUI event loop, introduce parameter
   structs while doing it, delete `too_many_arguments` allows as they go.
6. **#8 / #10** structural cleanups, opportunistically per touched file.
7. Low items as drive-by fixes; add a CI ratchet for `too_many_arguments`.

Constraints per AGENTS.md: one logical change per commit, Conventional Commits,
`cargo fmt` + clippy `-D warnings` + `cargo test --locked` green before each.
