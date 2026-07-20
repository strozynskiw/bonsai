# Release-binary performance baselines

These JSON files are reviewed outputs from the ignored native performance harness in
`tests/performance_baseline.rs`. A baseline is comparable only when its target, runner
class, profile, and Rust toolchain match the current run. Reports contain numeric
aggregates, raw samples, source and binary identities, and no prompts, code, headers,
credentials, environment values, stderr, or absolute workspace paths.

Run the harness from a clean checkout with a release build:

```sh
BONSAI_PERF_REPORT=target/perf/$(rustc -vV | sed -n 's/^host: //p').json \
BONSAI_PERF_SAMPLES=5 \
BONSAI_PERF_RUNNER_CLASS=local-native \
cargo test --release --locked --test performance_baseline -- \
  --ignored --nocapture --test-threads=1
```

Use a stable `BONSAI_PERF_RUNNER_CLASS` label for one comparable hardware/runner class;
it defaults to `local` for ad hoc runs and is part of the fail-closed comparison identity.

To measure an extracted official RC artifact instead of Cargo's freshly built binary,
also set `BONSAI_PERF_BINARY=/absolute/path/to/bonsai`. To compare against a reviewed
reference, set `BONSAI_PERF_BASELINE` to the matching file in this directory.

The harness records fresh-home and returning-home startup to the first parsed visible TUI
frame, idle CPU from the process CPU-time delta across a settled three-second window and
peak RSS during that window, headless spawn-to-first-assistant-output, final
shared-snapshot persistence, exact binary bytes and SHA-256, provider-reported context
growth and cache reuse, and deterministic representative-task token cost. Noisy system
metrics fail only when both their relative and absolute material margins are crossed;
deterministic token, cache, and cost metrics use tighter margins.

Baseline changes are never accepted automatically. Review the raw samples, runner class,
toolchain change, binary diff, and any violations before replacing a JSON file. A target
without a checked-in baseline remains report-only in CI until its first native run is
reviewed. The release workflow measures the exact binary it publishes; RC qualification
then proves the archived binary has the report's SHA-256 and size. Local reruns against a
downloaded binary are diagnostic and do not replace that hosted-runner evidence.
