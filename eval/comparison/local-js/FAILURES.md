# Baseline trajectory findings

Read-only diagnosis of the retained 60-run DeepSeek Flash baseline. These findings
are hypotheses for general harness improvements, not permission to tune answers
or evidence of a causal win from changing prompts or budgets.

| Bonsai run | Retained sequence | Outcome |
| --- | --- | --- |
| JSON Pointer, attempt 1 | One implementation write; a failing non-slash pointer assertion was followed by repeated edits to temporary tests, including changed expectations, rather than a solution repair | 12 requests; exhausted; independent check failed |
| JSON Pointer, attempt 2 | Correct implementation, incorrect self-test (`/a~1` decodes to `a/`, not `a/b`); repeated probes and add/remove comment edits | 12 requests; exhausted; all 17 independent checks passed |
| Layered config, attempt 1 | Repeated self-tests/prototype probes, including an incorrect expectation about `__proto__b`; last-turn edit made primitive values count as plain objects; no post-edit execution | 12 requests; exhausted; clone recursed until stack overflow |
| CSV, attempt 3 | Self-tests twice required SyntaxError for nonstrings, but later assertions were changed to TypeError rather than fixing implementation | 7 requests; completed; independent error-type check failed |

The specification already stated the relevant contracts. Hidden checks did not
introduce the missing requirements. Matching DSH attempts also made some wrong
self-tests; its CSV run repaired the exception type, and its layered-config run
continued beyond 12 steps. Do not attribute the whole gap to model quality or
compare budgets as if they were equal.

## General mechanisms to investigate

- **Budget-aware execution:** `--max-turns` counts model iterations, including
  reads, failed tests, edits and final responses. Provide early remaining-budget
  feedback and prioritize final verification, without silently exceeding caps.
- **Progress and recovery:** measure meaningful repair or new diagnostic evidence,
  not merely a successful read/grep. A fixed ten-turn stall threshold is too late
  for a twelve-turn job. Test repeated failures, read loops and last-turn edits.
- **Verification provenance:** preserve a failed assertion and its contract until
  resolved; modifying test expectations must not itself establish correctness.
  Distinguish declared project tests from disposable model-authored probes.
- **Verification observation:** current source command recognition does not treat
  `node test.mjs` as an observed verification command. That is a telemetry gap,
  not proof no tests ran. The baseline's automatic verification policy was disabled.

Potential source areas: `src/agent/run_loop/coordinator.rs`,
`src/agent/run_loop.rs`, `src/agent/verification.rs`, and `src/headless/mod.rs`.
Current source is not proof of exact historical binary behavior; the experiment
manifest pins the actual binary hash. Revalidate source mechanisms before edits.

## Evaluation discipline

Keep this baseline immutable. Test changes first with offline regressions covering
loop recovery, final-budget behavior and verification provenance. Then use fresh
held-out tasks and explicitly matched budgets. Report completion, artifact
correctness, requests, latency and cost separately; do not claim superiority by
raising only Bonsai's limit or leaking the independent checker to it. Any new
paid run requires fresh authorization.

Raw evidence remains in ignored `target/comparison/suite-1792393470751/runs/`:
`execution.json`, `checks.json`, `result.json`, workspace snapshots, and private
DSH session exports. The committed sanitized baseline preserves every attempt's
identity and result, not those private session bytes.
