# Eval harness

`bonsai eval` runs source-controlled fixture tasks through the **normal agent
and tool loop**, grades the resulting worktree, and writes a JSON report. It is
the release-gating correctness and efficiency instrument: mock mode is fully
deterministic and needs no credentials, so it runs in CI on every change.

This page is the orientation; [`eval/README.md`](../eval/README.md) is the
authoritative reference for the suite schema, graders, budgets, baselines, and
adapters — keep that file the single source of truth when they disagree.

## Running

```sh
bonsai eval                                  # default: mock mode, default suite
bonsai eval --mode mock --json               # print report JSON to stdout
bonsai eval --suite eval/suites/m2_10.toml --task t01_readme_agentic
bonsai eval --mode live --provider anthropic --out target/eval-live
```

- **Mock mode** (default) scripts the provider's tool calls from the suite
  file, but everything downstream — batching, read-before-write guards,
  diffs, token accounting, compaction — is the real agent path.
- **Live mode** uses a real provider: `--provider`, then `BONSAI_PROVIDER`,
  then the saved current provider (the same selection order as
  [headless mode](headless.md)). `--model` and `--effort` are live-only
  overrides, recorded in the report.
- `--fail-on-task-failure` makes task, grader, and budget failures exit
  nonzero — use it in CI and release gates. Harness, suite, CLI, fixture, or
  auth/config errors always exit nonzero regardless.
- `--baseline <path>` compares the run against a versioned baseline profile
  (exact suite/provider/model/effort identity) and fails below the
  correctness floor or above any configured efficiency tolerance.

Each run writes `target/eval/<run-id>/report.json` plus a CI-friendly
`summary.md`, and keeps an isolated `storage/bonsai.db` for forensics —
`eval/README.md` includes ready-made SQL for cache and read-efficiency
regressions.

## Suites

Suites are TOML files: an `id`, a `seed`, and `[[tasks]]` entries with a
fixture directory, a prompt, a mock script, and graders. Beyond the simple
read/write shorthand, tasks can exercise release scenarios: raw tool-call
batches with malformed arguments, deterministic cancellation and resumption,
truncated provider responses, forced compaction with a mock context window,
prompt-cache simulation with forbidden request substrings, and
shared-workspace tasks that run a second real agent session against the same
worktree to verify the workspace lock and change attribution.

Three grader types verify outcomes:

| Grader | Checks |
| --- | --- |
| `test-pass` | A trusted command exits 0 in the copied worktree within `timeout_secs` |
| `file-state` | A relative path's existence/absence, required/forbidden text, or exact equality with an expected file |
| `assertion` | Required/forbidden text in the final assistant output |

Suite-level `[budgets]` (overridable per task) bound logical turns, provider
attempts, duration, prompt/completion/uncached tokens, cost, per-turn
completion size, missing usage rows, re-reads of unchanged paths, and
post-warmup cache percentage.

## Release and efficiency suites

- `eval/suites/release_gating.toml` — transport, cancellation, batching,
  permissions, compaction, and shared-workspace safety.
- `eval/suites/language_acceptance.toml` — dependency-free Rust, TypeScript,
  Python, and Go repositories whose initial implementations fail their native
  tests; the agent must inspect, fix, and pass a clean re-run.
- `eval/suites/task_todo.toml` and `eval/suites/read_efficiency.toml` — the
  end-to-end efficiency benchmarks. Run both on Codex and at least one
  non-Codex transport before changing read reuse, cache shaping, retry, or
  batching behavior.

## Versioned baselines

Baseline files (`eval/baselines/*.toml`, `schema_version = 1`) pin a
suite/provider/model/effort profile with a `score_percent` correctness floor
and optional efficiency tolerances (token growth, cost growth, duration
growth, cache-reuse drop, repair-turn increase). Omitted tolerances stay
diagnostic; configured gates fail closed when a measurement is missing.

## External benchmark adapters

`bonsai eval adapter run` executes the normal structured headless surface
inside a workspace prepared by an external harness (SWE-bench, Harbor,
Terminal-Bench 2), with every policy and budget explicit in a versioned JSON
request. Adapters never install harnesses or download datasets, and benchmark
content enters bonsai as untrusted data, not instructions. Pinned upstream
commits and the result schemas live in
[`eval/README.md`](../eval/README.md#external-benchmark-adapters).

## Adding a task

1. Add or reuse a fixture directory under `eval/suites/fixtures/`.
2. Add a `[[tasks]]` entry with a unique id and prompt.
3. Add a mock script that reads existing files before writing them
   (the read-before-write guard is live in mock mode too).
4. Add graders that verify observable worktree state and final output.
5. Run `cargo run --locked -- eval --mode mock --task <id> --json`.
