# Paid local coding comparison: Bonsai vs DeepSeek Harness

## Result

Both harnesses fixed the same Retry-After parser bug on their first paid attempt,
passed six visible assertions plus **35 independent checks each**, and changed
only the permitted `retry.mjs` file. Independent verification required successful
process exits and checked Bonsai's structured completed status and DSH's decoded
compressed durable root outcome. No paid attempt was retried.

| Successful execution | Bonsai | DeepSeek Harness |
| --- | ---: | ---: |
| Wall time, including CLI startup | 7.998 s | 10.193 s |
| Model requests | 5 | 5 |
| Input tokens, including cache reads | 60,448 | 57,244 |
| Output tokens | 743 | 744 |
| Total tokens | 61,191 | 57,988 |
| Cache-read tokens | 47,616 | 50,688 |
| Local cost estimate | $0.005029 | Unavailable |
| Actual provider-billed cost | Unknown | Unknown |

Bonsai finished about 2.20 seconds sooner in this sample; DSH used 3,203 fewer
reported tokens. This single task does not establish an overall winner. The
native protocols, system prompts, tool sets, permission modes, output budgets,
cache warmth and serial execution order differ. DSH's separately reported
uncached input and cache-read fields are summed for the inclusive input column.
Unknown billed cost is not zero and the local estimate is not a hard cap.

## Task and verification

Both received identical initial bytes, specification, six visible assertions,
and prompt:

> Fix the bug in retry.mjs according to README.md. Inspect the files, implement
> the complete contract, and run node test.mjs. Modify only retry.mjs. Do not
> delegate or install dependencies. Report briefly.

The contract accepts only trimmed ASCII decimal digit strings, preserves leading
zero semantics, clamps to 120 seconds even for arbitrarily long strings, and
returns null for all other values without coercion or exceptions. Independent
checks covered whitespace, zero, boundaries, long digit/zero strings, fractions,
signs, exponents, HTTP dates, Unicode digits, junk, primitives, arrays, objects,
BigInt, Symbols and an object whose coercion throws. The checker was held outside
the task workspaces; local native processes are not a security-isolated benchmark.
README and visible-test file hashes remained unchanged; no extra workspace files
or symlinks were present.

Bonsai stripped leading zeros and checked digit length before numeric conversion.
DSH accumulated decimal digits and returned the cap once reached. Both validated
the complete string before conversion and satisfied every independent assertion.

The checker rejected the initial buggy implementation and accepted a reference
implementation **before any paid call**. An earlier checker preflight failed due
to an escaped-whitespace fixture error; it stopped before either harness launched.
That preflight directory is retained and excluded from paid attempt counts.

## Runtime and scope

- Model: DeepSeek Flash, thinking off, using the project `.env` key in-process.
  Secret values were neither printed nor placed in argv. Agent environments were
  selected explicitly rather than inheriting the developer's shell credentials.
- Bonsai release binary: version 0.4.2; SHA-256 retained in `experiment.json`.
  Native DeepSeek Chat, eight-turn limit, autonomous mode, private home/workspace.
- DSH clean commit: `687f282e8625c6b8d11ff58211a90524be2e4317`, Node v24.6.0,
  native Messages endpoint, workspace-write mode, output limit 4,096 tokens.
- One serial attempt per harness, Bonsai then DSH, 120-second process-group limit
  each, no automatic harness retries. Provider-internal retry behavior is native.
- No matching launcher/agent process remained in the post-run process listing.
- **Not TB4:** no Linux runner qualification, benchmark trial, external billing
  observer, calibrated request equivalence or benchmark score is claimed.

## Retained local artifacts

Under ignored/private `target/comparison/`:

- `paid-coding-1792390884467/`: experiment identity, reference/baseline checker
  evidence, per-harness stdout/stderr, results, before/after hashes, independent
  and visible checker results, and DSH compressed-session export.
- `paid-coding-1792390884467/verified-comparison.json`: independently verified
  outcomes and normalized token usage; actual provider billing remains null.
- `paid-coding-1792390872928/`: failed offline checker preflight, no paid calls.
- `paid-coding-pair.mjs`: final local runner and task/check definitions.
- `verify-paid-coding.py`: offline outcome, hash and durable-usage verification.

The runner passed `node --check` and executed both paid branches. The independent
project-environment Python verifier passed. Application source was unchanged;
Rust tests were not rerun because they do not validate this local experiment.
