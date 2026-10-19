# Paid 10-task suite rerun at 98d7ec8

Completed a fresh authorized 60-run comparison: ten unchanged tasks, three
attempts per task per harness, DeepSeek Flash with thinking off. All attempts
were retained; none was retried. This remains a custom local JavaScript suite,
not TB4, a sandbox, or a general agent ranking.

| Metric (30 attempts per harness) | Bonsai baseline | Bonsai rerun | DSH baseline | DSH rerun |
| --- | ---: | ---: | ---: | ---: |
| Completed successes | 26/30 | 28/30 | 30/30 | 29/30 |
| Correct artifacts within file scope | 27/30 | 29/30 | 30/30 | 29/30 |
| Median time including startup | 8.892 s | 8.186 s | 13.774 s | 11.920 s |
| Mean time | 11.209 s | 9.375 s | 16.423 s | 14.033 s |
| Nearest-rank p95 | 25.327 s | 20.810 s | 30.762 s | 25.151 s |
| Model requests | 168 | 155 | 244 | 204 |
| Total reported tokens | 2,255,443 | 2,000,068 | 3,206,025 | 2,548,126 |
| Local cost estimate | $0.180784 | $0.164740 | unavailable | unavailable |
| Actual billed cost | unknown | unknown | unknown | unknown |

Bonsai's completed success rate is 93.3%, versus DSH's 96.7% in the rerun.
Both deliver correct code in 96.7% of attempts. Bonsai's median time is about
31% lower than DSH's in this sample. Neither accuracy difference nor before/after
change establishes a causal improvement with this small, non-controlled suite.

## Rerun failures

- Bonsai CSV, attempt 1: correct artifact, but `budget_exhausted` and exit 1.
- Bonsai layered config, attempt 3: `budget_exhausted`, exit 1; default/no-argument
  config call returned undefined, failing an independent check.
- DSH async worker pool, attempt 1: completed and exit 0, but an independent
  assertion failed. Completion text was not accepted as proof of correctness.

Every run respected final workspace file restrictions. No matching launcher or
agent processes remained in the post-run process listing.

## Provenance and interpretation

Fresh warning-free release build from source commit `98d7ec8`; raw experiment
manifest records the exact binary SHA-256, which differs from the original
baseline. Task digest, Node v24.6.0 and clean pinned DSH revision
`687f282e8625c6b8d11ff58211a90524be2e4317` match the baseline. The same prompts,
order schedule and budgets were used (Bonsai 12 turns, DSH 4,096 output tokens,
120-second process limit each). Native protocols, permission modes, prompts,
cache warmth and unequal turn budgets remain confounders.

Changes since the baseline hardened provider-stream capture/replay and evaluation
correctness; the proposed budget-aware recovery and verification-provenance
improvements were **not implemented**. Better rerun numbers therefore cannot be
credited to those proposed fixes. Model sampling, provider/cache state and the
changed build may contribute; this experiment does not isolate them.

## Evidence and verification

Private/ignored raw artifacts: `target/comparison/suite-rerun-98d7ec8/`.
`verified-suite.json` contains all independently verified outcomes, normalized
usage and per-task summaries; `manifest.json` identifies execution inputs.
The frozen original baseline in `eval/comparison/local-js/baselines/` is unchanged.

Before paid execution: warning-free release build, six offline process tests,
baseline consistency check, all ten reference passes and all ten buggy assertion
failures. Three rounds executed once each, completing 20 attempts each.
After execution, the repository verifier rechecked all 60 identities, hashes,
file scopes, independent solution tests, execution limits and DSH durable root
outcomes. No new paid inference was used for verification.

Actual billing remains unknown. Time limits are not dollar caps. No application
source was edited for this rerun, and no superiority claim is supported.
