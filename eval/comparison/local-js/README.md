# Local JavaScript harness comparison

A custom 10-task coding suite, **not Terminal-Bench 4**, a general ranking, or a
security sandbox. Tasks and references were frozen before the paid baseline.
Each task permits changes to `solution.mjs` only; some read additional modules.
`tasks.json` preserves the exact executed source/check bytes (no unescaping step).

## Offline checks (no API key or inference)

From the repository root, with Node 24+ and a built Bonsai release binary:

```sh
node --test eval/comparison/local-js/process.test.mjs
node eval/comparison/local-js/check-baseline.mjs
node eval/comparison/local-js/run.mjs --preflight target/comparison/new-preflight
```

Preflight creates a **new** output directory, checks every reference passes and
every buggy input fails an assertion, and freezes task digest, binary SHA-256,
DSH revision, Node version, model, timeouts and attempt count. Existing directories
are never overwritten. Building the binary uses `cargo build --release --locked`.

The sanitized `baselines/deepseek-flash.json` includes all 60 attempt identities,
metrics and failure summaries. The baseline checker recomputes consistency; it
cannot re-prove executions without the private raw artifacts. Its provenance and
limitations are documented in `../../evidence/deepseek-flash-ten-task-suite.md`.

To independently regrade a retained raw run using a project Python environment:

```sh
target/tb4-harbor-venv/bin/python eval/comparison/local-js/verify.py \
  target/comparison/suite-1792393470751 \
  --output target/comparison/new-verified-suite.json
```

The verifier checks all 60 identities, final file hashes and permitted changes,
re-executes independent tests without credentials, and validates DSH's durable
root outcomes. Output must be new. It imports only Python standard library and
the repository DSH outcome helper. Run without Python optimization (`-O`), since
validation uses assertions. Raw run directories, homes, databases and session
logs remain private/ignored and are not necessary for the offline consistency
check or a new preflight.

## Paid execution — explicit authorization required

Do not use `execute` merely to verify packaging. It invokes real autonomous
agents with paid provider access. Private homes and narrow environment selection
reduce accidental exposure, **not filesystem/network isolation**. Use a disposable
machine without unrelated secrets. There is no enforced dollar cap or external
billing observer; the 120-second process-group timeout is not a spending cap.
Subprocess output retention is capped at 4 MiB combined; exceeding it terminates
and reaps the process group with an explicit `outputLimited` result. Offline
checker execution uses the same bounded helper.

Prerequisites:

- Project `.env` with `DEEPSEEK_API_KEY` (never put values in argv or commits).
- Built `target/release/bonsai` matching the preflight hash.
- Clean installed DSH checkout at commit
  `687f282e8625c6b8d11ff58211a90524be2e4317`, with its pinned pnpm dependencies
  already installed. Default path is
  `target/comparison/deepseek-harness-687f282e86`; override via `TB4_DSH_RUNTIME`.
- Node 24+, native Chat and Messages API connectivity.

After fresh authorization for 60 runs, execute each round **once** in the same
preflight directory, waiting for it to finish before starting the next:

```sh
SUITE_ROUND=1 node eval/comparison/local-js/run.mjs execute target/comparison/new-preflight
SUITE_ROUND=2 node eval/comparison/local-js/run.mjs execute target/comparison/new-preflight
SUITE_ROUND=3 node eval/comparison/local-js/run.mjs execute target/comparison/new-preflight
```

For an explicitly authorized Bonsai-only validation, set `SUITE_HARNESSES=bonsai`
for both preflight and every execution round. Selection is frozen in the manifest;
the verifier requires exactly those identities (30 instead of 60). This changes
only harness selection, never task/check bytes or per-attempt budgets. The default
remains both harnesses. Run `node --test eval/comparison/local-js/harnesses.test.mjs`
to verify selection validation.

Each round schedules one attempt per task per selected harness; order alternates. A durable
reservation prevents identity reuse: interrupted/failed identities remain occupied.
No resume/retry logic is provided. Preserve partial runs and do not relabel retries
as original attempts. Provider-internal retry behavior remains native.

Baseline budgets intentionally remain visible: Bonsai `--max-turns 12`; DSH
output limit 4,096 tokens, no matched turn cap. Native transports, prompts, tools,
permissions, cache warmth and budgets differ. Compare correctness **and** completed
runs separately. Do not claim improvements from simply increasing one harness's
budget or exposing independent checks to the agent; use held-out tasks to check
generalization.
