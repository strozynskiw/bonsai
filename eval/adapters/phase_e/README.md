# Phase E — external benchmark qualification (SWE-bench Verified)

This directory holds the **deliberately-remote** glue for Phase E of
[`plans/external_benchmark_adapters.plan`](../../../plans/external_benchmark_adapters.plan).
The Bonsai adapter (`bonsai eval adapter run`) owns only the agent run and
prediction extraction. This driver owns the externally-owned lifecycle the
adapter intentionally does not: dataset access, host workspace provisioning at
the pinned base commit, request-batch generation, invocation of the pinned
official grader, and normalization of the grader report.

## This is NOT normal CI, and NOT for casual local runs

Both the plan and `ROADMAP.md` require Phase E to run on an explicitly
provisioned, disk-capable runner. The `grade`/`all` stages **download the
SWE-bench Verified dataset and pull one multi-GB Docker image per instance**,
and the `run` stage **spends real provider tokens**. Nothing here runs in
Bonsai CI or is triggered automatically.

## Prerequisites (all required for a live run)

1. **Prebuilt bonsai binary** — `cargo build --release --bin bonsai`
   (driver defaults to `target/release/bonsai`).
2. **Docker** running, linux/amd64 image support, and ample free disk
   (each instance image is multi-GB; on Apple Silicon the grader images run
   under x86 emulation — slow).
3. **An env-key-backed provider credential exported in the shell.** The
   adapter launches the child bonsai with a *fresh temporary `BONSAI_HOME`* for
   determinism, which **strips your normal credential store**. OAuth/keyring
   providers (including the config default **codex**) will fail as
   `auth_config_failure`. Use a provider whose key is read from the
   environment and export it, e.g.:

   ```sh
   export ANTHROPIC_API_KEY=...        # then --provider anthropic (default)
   # or OPENAI_API_KEY / OPENROUTER_API_KEY / ZAI_API_KEY / ... with a matching --provider
   ```
4. **`uv`** (recommended) to run the script with its pinned deps, or a Python
   env with `datasets` and `swebench` installed.

## Offline wiring check (safe — no network / Docker / spend)

Proves the driver generates a request the real adapter accepts, using a
synthetic instance and a stub bonsai:

```sh
uv run eval/adapters/phase_e/run_canary.py selftest
```

## Live canary (crosses every boundary above — opt-in only)

Pick a provider whose key is read from the environment (see Prerequisite 3) and
export it. Two worked examples:

```sh
# Anthropic (reasoning models take an effort label):
export ANTHROPIC_API_KEY=...
uv run eval/adapters/phase_e/run_canary.py all \
    --provider anthropic --model anthropic/claude-sonnet-4-5 --effort high \
    --run-id bonsai-canary-001

# Xiaomi MiMo coding plan (tp- key). MiMo has no reasoning options, so it
# REJECTS an explicit none/high — use `--effort default`:
export MIMO_CODING_PLAN_API_KEY=tp-...
uv run eval/adapters/phase_e/run_canary.py all \
    --provider mimo-coding-plan --model mimo-coding-plan/mimo-v2.5-pro \
    --effort default --run-id bonsai-canary-001
```

(MiMo pay-as-you-go `sk-` keys instead: `MIMO_API_KEY`, `--provider mimo
--model mimo/mimo-v2.5-pro`.)

Which instances run comes from `canary_instances.txt` (one id per line). Override
inline without editing the file — repeatable:

```sh
    ... run_canary.py all ... --instance-id django__django-16082 \
                              --instance-id django__django-12419
```

### Stage by stage

`all` = these four in order; run them individually to inspect or to stop before
the heavy Docker grade:

| Stage | Does | Cost |
| --- | --- | --- |
| `provision` | clone repo at base commit + emit `requests.json` | network (git) |
| `run` | adapter runs bonsai → `predictions.jsonl` | provider tokens |
| `grade` | official SWE-bench harness scores the patch | dataset + multi-GB image pulls |
| `import-report` | grader report → `scorecard.json` + suggested baseline profile | none |

`provision` + `run` alone gets you the patch and the redacted sidecar with no
Docker and no dataset pull.

## Where results land (under `--out`, default `target/eval/phase-e/`)

- `adapter/predictions.jsonl` — the official SWE-bench prediction(s)
- `adapter/<instance>-<key>/bonsai-sidecar.json` — redacted metrics: tokens,
  cache reuse, terminal state, patch digest (never contains the key)
- `scorecard.json` — normalized resolved/total + score; a ready `eval/baselines`
  profile is also printed to the terminal for you to review before committing

`--help` on the script or any stage lists every flag.

## Pins

`SWE_BENCH_HARNESS_COMMIT` / `SWE_BENCH_PREDICTION_SCHEMA_COMMIT` in
`run_canary.py` mirror `src/eval/adapters/contract.rs`. The adapter rejects a
mismatch before launching any agent — update both together when the upstream
pin moves.

## Terminal-Bench 2.0 / Harbor

The Harbor agent adapter lives at [`../harbor/`](../harbor). Harbor owns its own
environment/verifier lifecycle; import its `TrialResult` with
`bonsai eval adapter import-harbor --result <path> --out <path>`. A dedicated
Harbor canary driver is intentionally out of scope for this SWE-bench driver.
