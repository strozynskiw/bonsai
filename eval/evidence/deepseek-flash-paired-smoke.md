# Live Bonsai vs DeepSeek Harness — local paired smoke

## Outcome

Both harnesses completed the same real-provider read–write–read task with
DeepSeek Flash and thinking disabled. An independent byte check confirmed
`task.txt` contained exactly `value=after\n`; neither successful workspace
contained any other file. This is a local smoke, **not TB4 or a performance
ranking**.

| Successful execution | Bonsai | DeepSeek Harness |
| --- | ---: | ---: |
| Exact-byte task check | Pass | Pass |
| Exit code | 0 | 0 |
| Wall time, including CLI startup | 6.269 s | 7.659 s |
| Model requests / durable attempt records | 4 | 4 |
| Total input tokens, including cache reads | 45,672 | 43,772 |
| Output tokens | 161 | 169 |
| Total tokens | 45,833 | 43,941 |
| Cache-read tokens | 33,920 | 42,240 |
| Local cost estimate | $0.001964 | Unavailable |
| Provider-billed cost | Unknown | Unknown |

DSH's `inputTokens` excludes its separately reported `cacheReadTokens`; the
table sums those fields for comparison with Bonsai's inclusive prompt count.
These are provider-reported usage samples, not an external billing statement.

## Task and environment

Identical prompt:

> Read task.txt, replace its complete contents with exactly "value=after"
> followed by a newline, read it again, and report briefly. Do not modify other
> files or delegate.

Each task started with `value=before\n` in a fresh private workspace and home.
The API key was loaded from the project `.env` in-process, never embedded in
command arguments or displayed. Children received a selected environment, not
the entire developer shell environment. Each run had a 120-second process-group
deadline, with termination escalation and remaining group cleanup. A subsequent
process listing found no matching test launcher or agent process.

Bonsai: release binary reporting version 0.4.2, native DeepSeek Chat transport,
`deepseek/deepseek-flash`, effort `none`, maximum eight turns, autonomous mode.
Its binary SHA-256 is retained in `experiment.json`.

DSH: clean source commit `687f282e8625c6b8d11ff58211a90524be2e4317`, Node v24.6.0,
headless CLI, native Messages transport at
`https://api.deepseek.com/anthropic`, thinking disabled, `maxTokens: 4096`,
explicit `workspace-write` sandbox mode and private workspace root.

The two agents use different native transports, prompt/tool sets, permissions,
and output budgets. Cache warmth differs. Runs were serial, not randomized or
counterbalanced. These differences preclude a controlled speed/cost ranking.
The small task is not evidence of general coding ability.

## Retained failures and corrections

DSH did not pass on the first launch. No failed attempt was overwritten:

1. `paired-smoke-1792389517252/dsh`: CLI exit 1 before task execution because
   `--session-id` requires an **existing** session. A fresh session must omit
   that flag and read its identity from the emitted `session` event.
2. `paired-smoke-1792389559955/dsh`: fresh session created; provider returned
   HTTP 404 because the native Messages transport was given the Chat base URL.
   Durable outcome is failure; usage/billing for the failed request is unknown.
3. `paired-smoke-1792389584602/dsh`: model completed but task failed because
   the headless sandbox was read-only. It correctly reported the inability to
   modify the file. This run consumed 55,823 reported tokens over five durable
   attempt records (1,545 uncached input, 53,760 cached input, 518 output).
4. `paired-smoke-1792389627247/dsh`: explicit workspace-write mode; task passed
   with a completed durable root session and complete usage telemetry.

DSH consumed 99,764 reported tokens across the read-only and successful runs,
plus unknown usage, if any, for the HTTP 404 attempt. The table above shows
only the successful execution, not total setup cost. Bonsai was not rerun after
its first successful attempt.

**Integration finding at capture time:** the Harbor DSH adapter supplied
`--session-id` for a new random identity. Synthetic CLI fixtures accepted that
shape but the real pinned CLI rejects it. This smoke corrected only its ignored
local launcher, not the Harbor adapter. Subsequent integration work repaired
the adapter to read the actual emitted fresh session identity, made synthetic
CLIs reject the old flag shape, and added a no-credential regression using the
real pinned CLI and compressed persistence exporter. All 43 integration tests
passed without skips. Fixture passes alone had not proved the live contract;
Linux runner qualification remains required before a live TB4 trial.

## Evidence and verification

Artifacts under `target/comparison/` (ignored/private local results):

- `paired-smoke-1792389517252/bonsai/`: passing Bonsai stdout, result and workspace.
- `paired-smoke-1792389517252/dsh/`: initial CLI failure.
- `paired-smoke-1792389559955/dsh/`: endpoint failure and decoded durable sessions.
- `paired-smoke-1792389584602/dsh/`: read-only task failure and durable sessions.
- `paired-smoke-1792389627247/dsh/`: successful DSH execution and durable sessions.
- `paired-smoke-1792389627247/verified-comparison.json`: offline-verified outcomes
  for **every** retained attempt, including failures.
- `live-pair.mjs`: bounded local launcher (final corrected version).
- `verify-live-pair.py`: independent outcome, byte, inventory and usage verifier.

`node --check` and real execution exercised the launcher. The independent
project-environment Python verifier passed, checked workspace inventories and
exact bytes, and parsed DSH through its compressed persistence service and
`parse_sessions` rather than trusting CLI `final` text. Both passing files have
SHA-256 `01a64e5e13635c2adcc8226d53b26b0d5e7d8789fd260e31ae73ebac38e9f51a`.

No TB4 trial, Linux isolation qualification, external billing observer or
benchmark score was run or inferred. No application source was changed for
this smoke; Rust builds/tests were not rerun because they do not exercise
this live local experiment.
