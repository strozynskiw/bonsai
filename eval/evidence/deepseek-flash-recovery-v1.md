# Recovery-v1 paid comparison

Completed the explicitly authorized 60 attempts: the unchanged ten-task local
JavaScript suite, three attempts per task per harness, DeepSeek Flash, thinking
off. All identities and failed attempts were retained without retries. This is
not TB4, a security sandbox, or a general agent ranking.

## Results

| Metric (30 attempts per harness) | Bonsai previous rerun | Bonsai recovery-v1 | DSH previous rerun | DSH recovery-v1 |
| --- | ---: | ---: | ---: | ---: |
| Correct completed runs | 28/30 | 27/30 | 29/30 | 29/30 |
| Correct in-scope artifacts | 29/30 | 27/30 | 29/30 | 29/30 |
| Median elapsed | 8.186 s | 7.624 s | 11.920 s | 12.354 s |
| Mean elapsed | 9.375 s | 8.470 s | 14.033 s | 15.819 s |
| p95, nearest rank | 20.810 s | 13.244 s | 25.151 s | 23.150 s |
| Model requests | 155 | 139 | 204 | 240 |
| Reported total tokens | 2,000,068 | 1,730,729 | 2,548,126 | 3,438,586 |
| Local estimated USD | 0.164740 | 0.156062 | unavailable | unavailable |

Actual billing is unknown. Reported input includes cached tokens; token totals
are not uncached cost. DSH layered-config attempt 2 took 115.443 seconds and 61
requests, strongly affecting its mean, requests and token totals.

All 30 Bonsai runs reported completed, with no turn-budget exhaustion; none used
more than eight requests. Nevertheless three delivered incorrect artifacts, so
this sample does **not** demonstrate a correctness improvement. Bonsai's median
was about 6.9% lower than its previous rerun and 38.3% lower than DSH's current
median, but this is descriptive, not causal evidence.

## Failures

- Bonsai stable-toposort attempt 1: independent check reports a missing expected
  exception; implementation explicitly deletes self-dependencies, bypassing
  self-cycle rejection. Visible tests passed.
- Bonsai async-worker-pool attempt 1: after a rejection, the final active worker's
  success path never settles the outer promise. Independent check exits 13 with
  unsettled top-level await. Visible tests passed.
- Bonsai async-worker-pool attempt 3: rejects before already-started work settles.
  Independent check's settled-work assertion fails. Visible tests and additional
  logging probes passed, but those probes did not assert the mixed-outcome case.
- DSH CSV attempt 2: nonstring input throws TypeError instead of required
  SyntaxError. Run exits zero and reports completion; independent checks fail.

The next reliability gap is contract coverage, especially error/termination
paths, not merely recovering from an observed failing test. No task answers or
checks were changed to improve the score.

## Attribution limits

The tested build includes uncommitted recovery advisories and Node verification
recognition, plus the current source at preparation time. Tasks, prompts,
schedule and budgets remain unchanged; model sampling/cache/backend state are
not controlled. Bonsai retains its twelve-turn cap; DSH has no matched turn cap.
No Bonsai attempt reached turn nine, where the near-budget notice begins for
this cap, so the budget-warning change was not exercised. Searches of available
Bonsai logs found no recovery-note strings, but those logs do not prove whether
notes reached model requests. Deterministic offline tests, not this paid sample,
provide affirmative advisory-delivery and hard-limit evidence.

## Provenance and verification

- Raw private results: `target/comparison/suite-recovery-v1/`.
- Previous rerun: `target/comparison/suite-rerun-98d7ec8/`.
- Task digest: `c137863c5186e8c8ec5cc7d5fe33342d3c1b67768fcc3d7fdd15d699c50b6e75`.
- Bonsai binary SHA-256: `9b846cee025e0113fa9085dd4f40523899fa8edb841f45dbbaeb4e2248f8c1e7`.
- DSH commit: `687f282e8625c6b8d11ff58211a90524be2e4317`.
- Node: `v24.6.0`.
- Fresh release build, six process tests, baseline consistency and preflight
  passed before paid execution. The binary hash remained unchanged afterward.
- `verify.py target/comparison/suite-recovery-v1` succeeded: checked all 60
  identities, scope/file hashes, execution limits and durable DSH outcomes, and
  independently re-executed every artifact's checker. Machine-readable results
  are in that directory's `verified-suite.json`; private homes/logs are excluded
  from repository evidence.
