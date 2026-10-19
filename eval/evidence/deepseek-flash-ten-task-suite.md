# Paid 10-task local suite: Bonsai vs DeepSeek Harness

## Outcome

Completed **60 paid runs**: 10 frozen tasks × 3 independent attempts × 2 harnesses,
using DeepSeek Flash with thinking disabled. No failed paid attempt was rerun or
discarded. DSH was more reliable on this suite; Bonsai was faster and used fewer
reported tokens. These are descriptive results from a custom JavaScript suite,
not Terminal-Bench 4 or a general coding-agent ranking.

| Metric, all 30 attempts per harness | Bonsai | DeepSeek Harness |
| --- | ---: | ---: |
| Strict successful runs | **26/30 (86.7%)** | **30/30 (100%)** |
| Correct delivered code and allowed file changes | 27/30 (90%) | 30/30 (100%) |
| Median agent wall time, including CLI startup | **8.892 s** | 13.774 s |
| Mean agent wall time | **11.209 s** | 16.423 s |
| Nearest-rank p95 agent wall time | **25.327 s** | 30.762 s |
| Sum of agent wall times | 336.281 s | 492.683 s |
| Model attempts/requests reported | **168** | 244 |
| Input tokens, including cache reads | 2,214,144 | 3,147,705 |
| Output tokens | 41,299 | 58,320 |
| Total reported tokens | **2,255,443** | 3,206,025 |
| Cache-read tokens | 1,813,248 | 3,062,784 |
| Local cost estimate, all attempts | $0.180784 | Unavailable |
| Actual provider-billed cost | Unknown | Unknown |

Strict success requires passing independent checks, respecting the file-change
scope, exit code zero, no timeout, and a completed terminal outcome. DSH's
terminal outcome and usage were decoded from its compressed durable sessions;
CLI `final` text alone was not accepted. Bonsai's terminal outcome was checked
against its structured headless result. DSH's uncached input and cache-read/write
fields are summed for inclusive input-token comparison.

All timings and usage include failed attempts. Timings exclude the external
post-run verifier and DSH persistence export. Fewer total tokens do not establish
lower cost: cache proportions differ substantially, actual invoices were not
observed, and DSH's cost remains unknown.

## Task results

Each count is strict successful attempts out of three, not best-of-three.

| Task | Independent assertions per passing attempt | Bonsai | DSH | Bonsai median | DSH median |
| --- | ---: | ---: | ---: | ---: | ---: |
| Interval union, validation, no mutation | 13 | 3/3 | 3/3 | 6.841 s | 11.695 s |
| CSV state machine and invalid-input handling | 17 | 2/3 | 3/3 | 8.307 s | 19.944 s |
| JSON Pointer, escaping and own-property traversal | 17 | 1/3 | 3/3 | 21.882 s | 13.207 s |
| Stable topological ordering | 11 | 3/3 | 3/3 | 7.559 s | 12.732 s |
| Async worker pool, concurrency and rejection | 13 | 3/3 | 3/3 | 13.598 s | 13.563 s |
| LRU cache and undefined values | 16 | 3/3 | 3/3 | 6.104 s | 9.527 s |
| Arbitrary-precision decimal money rounding | 18 | 3/3 | 3/3 | 9.282 s | 12.496 s |
| Layered configuration, deep cloning and unsafe keys | 9 | 2/3 | 3/3 | 8.812 s | 17.359 s |
| Strict semantic-version comparison | 26 | 3/3 | 3/3 | 9.935 s | 20.132 s |
| Chunked LF/CRLF line decoder | 11 | 3/3 | 3/3 | 7.558 s | 13.985 s |

There are 151 independent assertions across the 10 tasks. Checks stop at the
first failure within an attempt; failed attempts do not claim all 151 assertions
executed. No workspace scope violation was observed in any of the 60 runs.

### Retained failures

All four strict failures were Bonsai runs:

- **JSON Pointer, attempt 1:** exhausted its 12-turn limit; delivered code failed
  an invalid-pointer assertion by returning the document instead of undefined.
- **JSON Pointer, attempt 2:** delivered code passed the independent checks, but
  the CLI exhausted its 12-turn limit and exited 1. Counts as code-correct but
  not a strict successful run.
- **Layered config, attempt 1:** exhausted its 12-turn limit; external checks hit
  `RangeError: Maximum call stack size exceeded` in the delivered clone logic.
- **CSV, attempt 3:** CLI reported completed and exited 0, but external checks
  required `SyntaxError` for nonstring input and received `TypeError` instead.

No fixes or extra paid runs were applied after observing these failures. The
suite therefore measures the chosen harness configuration as well as model
behavior; Bonsai's explicit turn cap can affect completion independently of
correctness of the delivered code.

## Frozen procedure

- One small JS module task per fresh workspace, with identical specification,
  buggy source, visible tests and prompt for both harnesses. The config task also
  includes an immutable defaults module. Only `solution.mjs` may change.
- These are focused function/class implementation and repair tasks, **not** a
  multi-language, repository-scale, CLI or broad software-engineering suite.
- Prompt: “Implement the complete contract in README.md. Inspect the files, fix
  solution.mjs only, run node test.mjs. Do not install dependencies or delegate.
  Report briefly.”
- Before paid execution, all 10 references passed and all 10 buggy sources failed
  with assertion failures rather than import/syntax/setup errors.
- Three serial rounds, alternating first harness by task and round. Each attempt
  starts from original bytes with a private home and workspace. No automatic
  harness retries; native provider-internal retry behavior remains possible.
- 120-second process-group timeout per agent; timeout escalation and cleanup.
  No matching suite launcher or agent process remained in the post-run listing.
- Bonsai release 0.4.2, native DeepSeek Chat, effort none, 12-turn cap, autonomous
  mode, self-review off. Binary SHA-256 is recorded in the frozen manifest.
- DSH clean commit `687f282e8625c6b8d11ff58211a90524be2e4317`, Node v24.6.0,
  native DeepSeek Messages endpoint, thinking disabled, output cap 4,096 tokens,
  workspace-write mode. Fresh emitted session IDs bind durable exports.
- Key read from project `.env` in-process, not printed or placed in argv. Agent
  environments were explicitly selected, not inherited wholesale from the shell.

Tasks and independent checks were frozen together before paid execution. The
checker was outside the task workspace, but native local agents are not securely
isolated from its host filesystem; there is no benchmark-grade verifier secrecy
or isolation claim. Different native protocols, system prompts, tools, permission
modes, budgets, and cache warmth prevent attributing results solely to agent-loop
quality. Three attempts per task are correlated; the 30 runs are not 30 distinct
tasks. No confidence interval, statistical significance or broad ranking is
claimed. A 100% observed score here does not mean perfect general reliability.

## Retained evidence and verification

Private ignored directory: `target/comparison/suite-1792393470751/`.

- `tasks.json`, `manifest.json`, `preflight.json`: frozen specs, buggy/reference
  sources, visible/independent checks, task digest, binary identity and preflight.
- `runs/<task>-<attempt>-<harness>/`: exclusive reservation, private workspace,
  before/after hashes, complete redacted execution output, checker result, native
  profile, and DSH sessions/export result where applicable.
- `summary-round-1.json` through `summary-round-3.json`: all 60 raw outcomes.
- `verified-suite.json`: independently verified records, normalized usage,
  strict/code-correct outcomes, aggregate timings, task summaries and failures.
- `target/comparison/suite-tasks.mjs`, `run-suite.mjs`, `verify-suite.py`: local
  definitions, runner and offline verification scripts.

Frozen task digest (SHA-256 of JS JSON serialization):
`c137863c5186e8c8ec5cc7d5fe33342d3c1b67768fcc3d7fdd15d699c50b6e75`.

The runner passed syntax and reference/buggy preflight, then executed exactly
three 20-run rounds successfully. The independent Python verifier, using the
project's Harbor environment, rechecked all 60 delivered solutions, file hashes,
scope restrictions and durable DSH outcomes and verified token-total consistency.
Its initial inventory comparison incorrectly depended on Python-vs-JS filename
sort order; comparison was corrected to path/hash mappings, with no agent result
or task changed and no additional paid call.

No application source was changed, no full Rust verification was rerun for this
experiment, no new commit was made, and no TB4 benchmark or Linux runner
qualification is claimed.
