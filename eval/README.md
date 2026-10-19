# Eval Harness

`bonsai eval` runs source-controlled fixture tasks through the normal agent and
tool loop, grades the resulting worktree, and writes a JSON report.

## Local Bonsai vs DeepSeek Harness suite

The reproducible [10-task JavaScript comparison](comparison/local-js/README.md)
includes frozen tasks, an offline preflight, a paid runner, a raw-run verifier,
and sanitized metrics for every baseline attempt. It is a custom local suite,
not TB4 or a security sandbox. Start with its offline checks; paid execution
requires explicit authorization. [Failure diagnosis](comparison/local-js/FAILURES.md)
records the baseline limitations without changing task answers.

## Provider stream capture and keyless replay

Capture is opt-in and eval-only. Normal TUI, headless, and ordinary live/mock
runs are unchanged. This records normalized provider callbacks and outcomes,
not HTTP/SSE traffic; transport parsing is not tested by replay.

```sh
# Controlled live smoke: opt in only when spending provider quota is intended.
cargo run --locked -- eval --suite eval/suites/provider_stream_tools.toml \
  --mode live --provider openai --effort off \
  --record-provider-streams eval/recordings/smoke --fail-on-task-failure
# Use the same suite, seed and build; credentials are not needed for replay.
cargo run --locked -- eval --suite eval/suites/provider_stream_tools.toml \
  --mode replay --replay-dir eval/recordings/smoke --fail-on-task-failure
```

Only portable `openai-chat`, reasoning-off, text-only single-agent tasks are
supported. Native replay history, images, peer tasks, qualification/reviewer
providers and delegation are excluded. Capture and replay use the same local
heuristic estimator, with transport-prepared inputs and declared roots normalized
for counting. Replay does not open user credential/session storage, refresh the
catalog, construct a live provider, or call a remote tokenizer. Tools still run
normally: arbitrary shell/network tools are neither deterministic nor made safe
by provider replay. Use controlled local fixture tools and normal permissions.

Each suite/task/repetition owns a hash-named JSONL file with a versioned header,
ordered attempts (including errors before any callback), and a completeness
marker. Files are exclusive-created with private permissions; generated captures
belong in ignored `eval/recordings/`, outside workspace fixtures. Limits are
32 MiB per artifact and 4 MiB per attempt. Writes run off the async runtime;
cancelled/dropped attempts cannot acquire a false completeness marker.
Mismatches, extra calls, incomplete/malformed/version-unknown artifacts, write
failures and unused attempts fail the task even when its expected status is
error or cancellation. There is no fallback to a live provider.

Matching includes ordered messages/tools, request options, response provenance,
canonical/wire tool identity relationships, and normalized prepared local-count
inputs. Only declared workspace/output roots and allocated identities are
normalized; semantic prompt/tool changes fail before callbacks are emitted.
Replayed tool arguments are rebased into the new workspace.
The fixture header records the capture platform; replay uses that platform in
its isolated project context. User database paths are omitted from stream-mode
context, so fixtures do not depend on the caller's home directory. A platform
record does not make arbitrary shell commands portable across operating systems.

For controlled failures during capture, set `BONSAI_EVAL_STREAM_FAULT` to
`error-after-prefix:N` or `wait-for-cancellation:N`, where `N` is a positive,
one-based attempt number. The former emits a fixed prefix then a retryable 503;
the latter emits the prefix and waits for the task's existing
`cancel_after_provider_attempts`/`cancel_after_ms` control and timeout. Replay
uses the recorded outcome, no timing sleeps, and immediate retry backoff. These
are injected faults, not evidence that a real provider outage was reproduced.

Artifacts contain only allowlisted DTO fields: no authorization/count credentials,
raw payloads, signatures, encrypted/native reasoning or arbitrary diagnostics.
Recursive credential-field and shared secret-pattern checks run before disk
writes. If redaction would alter executable tool semantics or relationships,
capture fails rather than recording a different action. **Prompts and source can
still be confidential.** Review every header, request, callback, error, tool
argument/result, path and identity relationship manually before copying a capture
into `eval/fixtures/`; redaction is not a safe-to-share guarantee. Never promote
user databases or live sessions.

The checked-in fixtures are synthetic, reviewed local read/write, retry and
cancellation cases (not live-provider captures). Run them without keys:

```sh
cargo test --locked stream_
cargo run --locked -- eval --suite eval/suites/provider_stream_tools.toml \
  --mode replay --replay-dir eval/fixtures/provider_streams/tools --fail-on-task-failure
```

The `provider_stream_retry.toml` and `provider_stream_cancellation.toml` suites
use the corresponding `retry` and `cancellation` fixture directories. Offline CI
runs all three through `stream_checked_in_replay_suites_are_keyless`. To regenerate
synthetic fixtures deliberately, run
`BONSAI_GENERATE_TEST_STREAMS=1 cargo test --locked stream_round_trip`, then review
the full JSONL diffs; ordinary tests never rewrite checked-in artifacts.

## Experimental qualification matrix (#13)

```sh
cargo run --locked -- eval --suite eval/suites/language_acceptance.toml \
  --qualification --mode mock --out target/qualification
```

This expands all four language tasks into available/unavailable LSP cells for
all suite repetitions. Mock available cells use an embedded Python protocol
fixture, not installed language servers; unavailable cells force startup failure.
Native commands still execute: Cargo, npm, Python unittest, and Go tests.
Install Rust, Node (with TypeScript stripping support), Python 3.10+, and Go
before running. Independent scripted review shares the provider-attempt budget.
Any failed cell returns nonzero without `--fail-on-task-failure`.

`qualification.json` is the allowlisted, credential-pattern-redacted snapshot.
Do not upload the run directory: databases, worktrees, and `*-private-evidence.json`
are private forensic artifacts. Pattern redaction is not a guarantee that all
sensitive prose is removed; inspect any bundle before publication.

This is **not live model qualification**. Live qualification and legacy baseline
comparison are rejected on this experimental path. Source/fixture hashes are
recorded, but full environment provenance, reviewed live candidate promotion,
strict versioned comparison and stronger fixtures remain unfinished.
The mock-only workflow is `.github/workflows/qualification.yml`; it uses explicit
Node/Python/Go versions and the repository's stable Rust toolchain policy.
No live baseline is frozen. #13 remains open; authorized model/spend selection,
complete repeated live evidence, human review, and measured baseline freezing
must follow completion of those framework pieces.

## Commands

```sh
bonsai eval
bonsai eval --mode mock --json
bonsai eval --suite eval/suites/m2_10.toml --task t01_readme_agentic
bonsai eval --mode live --provider anthropic --out target/eval-live
cargo run --locked -- eval --mode live --provider opencode --model glm-5.2 \
  --effort default --suite eval/suites/task_todo.toml --fail-on-task-failure
```

Mock mode is the default. It uses deterministic scripted tool calls from the
suite and does not require provider credentials. Live mode uses `--provider`,
then `BONSAI_PROVIDER`, then the saved current provider, matching headless
selection. Eval run storage is isolated under the run output directory.
`--model` and `--effort` are live-only overrides and are recorded in the report.
Use `--fail-on-task-failure` in CI or release gates to make task, grader, and
budget failures exit nonzero. `--baseline <path>` selects an exact
suite/provider/model/effort profile, reports metric deltas, and exits nonzero
when the run falls below the profile's correctness floor.

## Suite Schema

Suites are TOML files:

```toml
id = "m2_10"
seed = 210
repetitions = 1

[[tasks]]
id = "t01_readme_agentic"
fixture = "fixtures/tiny_project"
prompt = "Update README.md."
profile = "full"

[tasks.mock]
read = ["README.md"]
final_response = "Updated README.md."
[[tasks.mock.write]]
path = "README.md"
content = "new content\n"

[[tasks.graders]]
type = "file-state"
path = "README.md"
contains = ["new content"]
```

`repetitions` runs every selected task in a fresh copied worktree, with stable
`<task>-run-N` report ids when the value is greater than one. It defaults to 1.
Tasks default to the `full` coding prompt; `profile = "smol"` exercises the
compact prompt and tool profile. A suite with tasks that omit `tasks.mock` is
live-only and must be run with `--mode live`.

Release scenarios may emit raw tool-call batches so malformed argument strings
and parallel calls traverse the normal agent path. A task can also declare an
expected terminal status and deterministic cancellation:

```toml
expected_status = "interrupted"
cancel_after_provider_attempts = 1

[tasks.mock]
final_response = ""
wait_for_cancellation = true
```

Completed tasks require graders. Expected `interrupted` or `error` tasks may
omit them; error tasks can additionally declare `expected_error_contains`.
Set `resume_after_interruption = true` with a cancellation trigger to continue
the same agent context after the scripted interruption. Provider-attempt
cancellation is deterministic; `cancel_after_ms` remains available for timing
scenarios. `truncated_responses = 1` injects one empty length-truncated provider
response before the normal script. `expected_user_message_count` asserts the
post-shaping provider request does not replay a human prompt during recovery.
`queued_messages` delivers deterministic human follow-ups through the same
in-flight queue used by the TUI, at the first normal response boundary. Use it
to qualify additive, superseding, and status-request steering. Set
`disable_episodes = true` when a pressure-compaction task must not be satisfied
by episode eviction first.
Long-context mock tasks can set `mock_context_window_tokens`, `min_compactions`,
and `tool_turn_content_chars` to force and verify automatic compaction without
large fixture files. `simulate_prompt_cache = true` reports cache reads from the
byte-stable prefix shared by consecutive requests, while
`forbidden_request_substrings` rejects obsolete context markers at the provider
boundary.

Shared-workspace tasks can pause the primary after a scripted tool turn, run a
second real agent session against the same worktree, then resume the original
context. The peer has its own prompt, mock script, read tracker, session id, and
changed-file expectations:

```toml
cancel_after_provider_attempts = 2
resume_after_interruption = true

[tasks.mock]
wait_for_cancellation_after_tool_turns = 1
final_request_contains = ["Other bonsai sessions in this project"]

[tasks.shared_workspace_peer]
prompt = "Update README.md from the peer session."
expected_primary_changed_files = ["primary-session.txt"]
expected_peer_changed_files = ["README.md"]

[tasks.shared_workspace_peer.mock]
final_response = "Peer complete."
```

Both agents use the normal workspace lock and stale-read guard. The task report
records each session id and its successfully mutated paths; an attribution
mismatch fails the task.

Task ids must be unique and path-like fields must be safe relative paths. The
default suite keeps fixtures under `eval/suites/fixtures/`; fixture paths are
resolved relative to the suite file directory.

Suite-level budgets apply to every task; a `[tasks.budgets]` table overrides
individual fields for one task. Supported limits are logical turns, provider
attempts, duration, prompt/completion/uncached tokens, cost, per-turn completion,
missing usage rows, reads per unchanged path, and post-warmup cache percentage:

```toml
[budgets]
max_logical_turns = 45
max_provider_attempts = 50
max_duration_ms = 900000
max_prompt_tokens = 1500000
max_completion_tokens = 150000
max_uncached_prompt_tokens = 450000
max_cost_micros = 2000000
max_completion_tokens_per_turn = 32000
max_missing_usage_turns = 0
min_post_warmup_cache_percent = 65
cache_warmup_turns = 5
max_reads_per_unchanged_path = 2
```

Duration, logical-turn, and provider-attempt limits stop a runaway task. Other
limits fail its budget report after the run, preserving the full diagnostic
record.

## Mock Mode

The mock provider emits normal model responses:

- one turn of `read` tool calls for `tasks.mock.read`
- one turn of `write` tool calls for `tasks.mock.write`
- one final assistant response from `final_response`

For release scenarios, `[[tasks.mock.tool_turns]]` replaces the read/write
shorthand. Each nested call has `name`, raw string `arguments`, and an optional
`id`; calls in the same turn are offered to the real batching policy together.

Because those are ordinary tool calls, read-before-write guards, diffs, token
accounting, and batching still run through the real agent path.

## Release Acceptance Suites

`release_gating.toml` covers transport, cancellation, batching, permissions,
compaction, and shared-workspace safety. `language_acceptance.toml` contains
dependency-free Rust, TypeScript, Python, and Go repositories whose initial
implementations fail their native tests. Each mock agent inspects the source,
edits it, runs the native command, and is graded by a second clean invocation:

```sh
cargo run --locked -- eval --mode mock \
  --suite eval/suites/language_acceptance.toml \
  --baseline eval/baselines/release-v1.toml --fail-on-task-failure
```

`intent_continuity.toml` deterministically qualifies additive and superseding
steering, status-then-continue behavior, pressure compaction, and episode recall:

```sh
cargo run --locked -- eval --mode mock \
  --suite eval/suites/intent_continuity.toml --fail-on-task-failure
```

`intent_authority.toml` is the complementary live-only matrix. It repeats
explanation, review, verification, monitoring, mutation, and established-pattern
engineering tasks three times in fresh worktrees across full and SMOL profiles.
Release qualification runs it on two model families and retains each run
directory; `report.json` records prompts, provider/model/effort, execution
policy, budgets, effects, changed files, usage, outputs, and grader artifacts:

```sh
cargo run --release --locked -- eval --mode live --provider codex \
  --suite eval/suites/intent_authority.toml \
  --out target/eval-live/intent-codex --fail-on-task-failure
cargo run --release --locked -- eval --mode live --provider anthropic \
  --suite eval/suites/intent_authority.toml \
  --out target/eval-live/intent-anthropic --fail-on-task-failure
```

The tagged-release workflow pins Node 22.18, Python 3.12, and Go 1.23 for this
suite; Rust uses the repository's stable toolchain. The TypeScript fixture uses
Node's built-in erasable-type support and has no npm dependencies.

## Versioned Baselines

Baseline files are source-controlled TOML with schema version 1. Profiles are
selected by exact suite, provider, model, and effort identity so results from
different inference configurations are never compared accidentally:

```toml
schema_version = 1

[[profiles]]
suite = "release_gating"
provider = "mock-eval"
model = "mock-eval-v0"
effort = "default"
score_percent = 100.0
allowed_score_drop_percent = 0.0
allowed_total_token_growth_percent = 5
allowed_cache_reuse_drop_points = 5
allowed_repair_turn_increase = 0
total_tokens = 402909
duration_ms = 298
cache_reuse_percent = 41
repair_turns = 0
```

`score_percent - allowed_score_drop_percent` is always the correctness floor.
Profiles may also gate material efficiency regressions with
`allowed_total_token_growth_percent`, `allowed_cost_growth_percent`,
`allowed_duration_growth_percent`, `allowed_cache_reuse_drop_points`, and
`allowed_repair_turn_increase`. Omitted tolerances leave that metric
diagnostic. Percentage tolerances and cache-drop points must be between 0 and
100; a configured cost or cache gate fails closed if either its reference or the
completed run lacks the measurement. Every comparison serializes the applied
policy beside its metric deltas and violations.

Falling below any configured floor or above any configured tolerance fails the
process whenever `--baseline` is present. Keep noisy wall-clock references
diagnostic unless the runner is stable enough for a deliberate bound; suite
budgets remain the absolute per-task safety limits. Unsupported schemas, invalid
profiles, duplicate identities, and missing exact matches fail closed before a
report is accepted.

## Graders

`test-pass` runs a trusted suite command in the copied worktree and passes when
it exits 0 before `timeout_secs`.

```toml
[[tasks.graders]]
type = "test-pass"
command = "sh scripts/check_evals.sh"
timeout_secs = 5
```

`file-state` checks a safe relative path for existence, absence, text content,
forbidden text, or exact equality with a suite-controlled expected file.

```toml
[[tasks.graders]]
type = "file-state"
path = "README.md"
contains = ["bonsai eval"]
not_contains = ["TODO"]
exact_file = "expected/readme.md"
```

`assertion` checks the final assistant output.

```toml
[[tasks.graders]]
type = "assertion"
contains = ["Updated README.md"]
not_contains = ["failed"]
```

`changed-files` checks the paths Bonsai observed the agent mutate. `exact = []`
is an explicit no-mutation assertion; `required` and `forbidden` provide subset
checks.

```toml
[[tasks.graders]]
type = "changed-files"
exact = ["src/lib.rs"]
```

`tool-effects` checks successful effect classes: `inspection`,
`workspace-mutation`, `command-execution`, `interaction`, `delegation`,
`external-access`, `local-state`, and the conservative `unknown` fallback.
`required_attempts` accepts an expected-failing operation as positive evidence;
`forbidden_attempts` rejects matching calls even when the tool or permission
path fails, exposing redundant questions and out-of-scope work.

```toml
[[tasks.graders]]
type = "tool-effects"
required = ["workspace-mutation", "command-execution"]
forbidden = ["external-access"]
required_attempts = ["inspection"]
forbidden_attempts = ["interaction", "external-access"]
```

Task failures are reported in JSON and do not make `bonsai eval` exit nonzero
unless `--fail-on-task-failure` is set. Harness, suite, CLI, fixture, or
auth/config errors always exit nonzero.

## Report Format

Each run writes `target/eval/<run-id>/report.json` and
`target/eval/<run-id>/summary.md` by default. The JSON report includes suite
id/path, repetitions, mode, provider/model, seed, score, per-task grader details, token
totals, cost when pricing is known, tokens per dollar, duration, and each
copied worktree path. It also includes suite-level cache reuse, verification
repair turns, and an optional versioned baseline comparison. The Markdown
summary is the CI-friendly scorecard: overall score, token/cost/latency/cache/
repair totals and deltas, reasoning selection, and per-task
score/budget/tokens/tokens-per-dollar. Each JSON task includes lane-aware usage
turns, finish reasons, reasoning size, inspection counters, changed files,
successful and attempted tool-effect classes, execution policy, and budget
metrics.

Use `--json` to also print the report JSON to stdout for CI ingestion.

## External benchmark adapters

`bonsai eval adapter` runs the normal structured headless surface inside a
workspace already prepared by an external harness. Requests are versioned JSON
objects (or arrays for a batch), and every provider/model/effort, autonomy,
network, turn, generation, output, tool, wall-time, and patch limit is explicit:

```sh
bonsai eval adapter run \
  --request /path/to/prepared-swe-request.json \
  --out target/eval/adapters

bonsai eval adapter import-harbor \
  --result eval/fixtures/adapters/harbor-result-v1.json \
  --out target/eval/adapters/harbor-result.json
```

The pinned contracts are:

- SWE-bench harness `f7bbbb2ccdf479001d6467c9e34af59e44a840f9`
- sb-cli prediction schema `b679692b8b7e274a6c89fd0842f25b02da4b9256`
- Harbor `2d3f78d55a703df2f76c005d7df44a5ce2d8adf5`
- Terminal-Bench 2 dataset `2fd12b88aafdd04a52c298e3940bcb189f9766d6`

An unknown request schema or upstream commit fails before Bonsai starts. The
SWE-bench adapter exports official `instance_id`, `model_patch`, and
`model_name_or_path` JSONL while keeping bounded, redacted Bonsai diagnostics in
a separate sidecar. Patch extraction includes tracked and untracked text and
rejects binary, symlinked, non-UTF-8, or oversized output. Completed results are
idempotently reused by benchmark/dataset/task/Bonsai-configuration identity;
`--force` intentionally reruns them.

The Harbor import-path adapter is
`eval.adapters.harbor.bonsai:Bonsai`. It requires a preinstalled Bonsai binary
and explicit policy/budget kwargs; see `eval/adapters/harbor/README.md`.

These commands never install Harbor or SWE-bench and never download datasets,
language runtimes, Docker images, or benchmark containers. Official scoring and
canary execution remain opt-in work for a disk-capable remote runner. Benchmark
instructions and repositories enter Bonsai as untrusted user/workspace data,
not system instructions or permission grants.

## External qualification handoff (#14)

Copy `eval/adapters/qualification-manifest.example.json` and freeze task IDs,
repetitions, immutable dataset revision, candidate revision/digest, execution
profile and budgets **before** running. The example is a fixture, not an approved
qualification selection; `django__django-11099` remains only a smoke example.

Only an explicitly approved remote Linux amd64 runner with Docker may download
or spend tokens. `eval/adapters/runner-requirements.txt` pins upstream source,
but is **not** a transitive lockfile: the operator must create and review a
hash-locked wheelhouse before use. Record installed VCS revisions from pip's
`direct_url.json`, Docker version, architecture, free disk against the manifest,
and presence (never contents) of the credential environment variable. Fresh
`BONSAI_HOME` cannot reuse OAuth/keyring credentials. Standard CI stays offline.

On that runner use the prepared Python environment, not `uv run` auto-install:

```sh
.venv/bin/python eval/adapters/phase_e/run_canary.py provision --out "$PRIVATE" --manifest "$MANIFEST" --approve-live
.venv/bin/python eval/adapters/phase_e/run_canary.py run --out "$PRIVATE" --manifest "$MANIFEST" --approve-live
.venv/bin/python eval/adapters/phase_e/run_canary.py grade --out "$PRIVATE" --manifest "$MANIFEST" --approve-live
.venv/bin/python eval/adapters/phase_e/run_canary.py import-report --out "$PRIVATE" --manifest "$MANIFEST"
```

Preflight reads profile/task selection from the manifest, verifies installed VCS
pins and the executable with `b3sum`, checks Docker, architecture, credentials and
disk. Install `b3sum` on the runner. Grading uses the pinned dataset snapshot.
Import checks task completeness and sidecar profile; never treat CLI flags as
proof of actual execution. Restart only with matching evidence; do not silently
rerun paid trials or mix candidate revisions. Run Harbor separately with the
pinned adapter and a preinstalled Bonsai binary; Harbor owns verifier and cleanup.

The versioned external scorecard in `src/eval/adapters/scorecard.rs` keeps each
suite's expected trial IDs, individual pass/fail/error outcomes and binary
Terminal-Bench rewards. Missing trials and infrastructure errors cannot qualify.
Ordinary verifier failures are complete benchmark outcomes. Public export uses
strict typed fields and rejects credential patterns and unknown private fields:

```sh
bonsai eval adapter import-scorecard --result private-import-bundle.json --out public-scorecard.json
# Human review must explicitly set reviewed=true after inspecting provenance.
bonsai eval adapter compare --candidate public-scorecard.json --out approved-baseline.json
```

The comparison is an offline RC gate: both suites, reviewed real evidence,
matching manifest/profile/pins/trials, and no score drop are mandatory. Missing
baseline returns nonzero. Candidate revision and binary version may differ;
execution policy/budgets may not. `manifest_digest` must identify the frozen
selection/profile, excluding candidate identity so RC revisions can be compared.
Baseline creation is a separate human-reviewed promotion, never automatic.

The import bundle contains `scorecard`, optional upstream-v2 `swe_report`, and
`harbor_trials` records (`id`, `task_id`, `repetition`, pinned `result` envelope).
Harbor IDs must be `<task_id>-run-<repetition>`; SWE IDs use official instance IDs.
Import resets `reviewed=false`, preserves binary rewards and checks missing trials.

**Preparation remains incomplete:** deterministic transitive runner locking,
fully verified Harbor run provenance, complete Harbor invocation and offline
end-to-end promotion tests still require implementation. Do not use manually assembled
scorecards as sufficient release evidence. Administrative closure of #14 does
not qualify a release or attest completion of the remaining work.

<!-- Remaining work after administrative closure of #14, at the user's request:
1. Freeze and review transitive dependency versions/hashes and a remote-runner
   wheelhouse; verify deterministic offline deployment.
2. Supply and verify the exact pinned Harbor invocation, candidate image setup,
   credential/profile propagation, and task/repetition provenance.
3. Complete the operator handoff and verify manifest -> both harness reports ->
   common import -> scorecard -> regression comparison end to end offline.
4. Only after explicit operator approval: run both remote canaries, review and
   publish redacted evidence, explicitly promote a real baseline, and verify
   the RC gate against frozen results. No live qualification or baseline exists
   merely because the issue is closed.
-->

Keep workspaces, DBs, transcripts, patches and raw grader reports private. Publish
only reviewed allowlisted evidence plus a limitations note under
`eval/evidence/0.5.0/`; automated redaction does not replace human review. Label
sample size, errors and canary limitations, never claim full benchmark results.

Offline Python checks need no external dependencies and must not install them:

```sh
python3 -m venv /tmp/bonsai-eval-offline
/tmp/bonsai-eval-offline/bin/python -m unittest eval.adapters.phase_e.test_import
```

## Efficiency Gates

`task_todo.toml` is the end-to-end implementation benchmark recovered from the
July failure sessions. `read_efficiency.toml` is a broad review task with known
findings and tighter turn/read budgets. Run each on Codex and at least one
non-Codex transport before changing read reuse, cache shaping, retry, or batching:

```sh
cargo run --locked -- eval --mode live --provider codex --model gpt-5.5 \
  --suite eval/suites/task_todo.toml --fail-on-task-failure
cargo run --locked -- eval --mode live --provider opencode --model qwen3.7-max \
  --effort default --suite eval/suites/task_todo.toml --fail-on-task-failure
cargo run --locked -- eval --mode live --provider opencode --model glm-5.2 \
  --effort off --suite eval/suites/task_todo.toml --fail-on-task-failure
cargo run --locked -- eval --mode live --provider opencode --model glm-5.2 \
  --effort max --suite eval/suites/read_efficiency.toml --fail-on-task-failure
```

The task suite keeps both an uncached-token ceiling and a 65% aggregate cache
floor. Warm OpenCode Anthropic turns normally report 90%+ cache reads, but
occasional backend-wide zero-cache turns make a 70% single-run aggregate
flaky even when the serialized prefix is stable. The uncached-token ceiling
still fails a sustained cache regression.

## Forensics

Every run has an isolated `storage/bonsai.db`. These queries diagnose cache and
read regressions without reconstructing custom SQL from the application schema:

```sh
DB=target/eval/<run-id>/storage/bonsai.db

sqlite3 "$DB" "
select lane_kind, lane_id, count(*) turns,
       sum(prompt_tokens) prompt,
       sum(coalesce(cache_read_input_tokens,0)) cache_read,
       sum(coalesce(cache_measured_input_tokens,prompt_tokens)) measured,
       round(100.0*sum(coalesce(cache_read_input_tokens,0))/
             nullif(sum(coalesce(cache_measured_input_tokens,prompt_tokens)),0),1) cache_pct,
       sum(inspection_executed) executed,
       sum(inspection_reused) reused,
       sum(inspection_rejected) rejected,
       sum(inspection_avoided_chars) avoided_chars
from usage_turns group by lane_kind,lane_id order by lane_kind,lane_id;"

sqlite3 "$DB" "
select canonical_path, admission_outcome, count(*) events,
       sum(returned_chars) returned_chars, sum(avoided_chars) avoided_chars
from read_evidence group by canonical_path,admission_outcome
order by sum(returned_chars) desc;"

sqlite3 "$DB" "
select seq,lane_kind,lane_id,lane_seq,status,finish_reason,reasoning_chars,
       rewrite_kind,local_reusable_prefix_percent,actual_cache_read_percent
from usage_turns order by seq;"
```

## Adding Tasks

1. Add or reuse a fixture directory under `eval/suites/fixtures/`.
2. Add a `[[tasks]]` entry with a unique id and prompt.
3. Add a mock script that reads existing files before writing them.
4. Add graders that verify observable worktree state and final output.
5. Run `cargo run --locked -- eval --mode mock --task <id> --json`.
