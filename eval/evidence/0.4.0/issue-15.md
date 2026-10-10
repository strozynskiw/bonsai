# Issue 15: four-target adversarial release qualification

Qualification date: 2026-10-10

Result: **partially qualified — one of four targets**. The aarch64-Darwin packed
artifact passed the full gate locally. x86_64-Linux, aarch64-Linux and
x86_64-Darwin remain **unqualified**: no native runner was available in this
session, and cross-target claims are not inferred from the local pass or from
release-profile unit tests.

## Qualification contract

- Source revision: `0d4520a11728f9b5c41a7483dff4925912318ad1` plus the uncommitted
  qualification working-tree patch (the artifact was packed from that tree, so
  the revision alone does not identify it)
- Candidate: `dist/bonsai-local-aarch64-apple-darwin.tar.gz` (repository-local
  equivalent of `bonsai-<tag>-<target>.tar.gz`)
- Archive SHA-256:
  `cf53f71b117e53c75859d6ff4b18c2d3a8fba8e2750394d45c404a94dd157d8c`
- Extracted binary SHA-256:
  `d1d95683adceb97626b22cb12812a9fbe54b3dcde545dc5c7d579fd685e3729a`
- Toolchain: `rustc 1.98.1 (48a229cea 2026-09-01)`
- Runner: `macOS-26.6.2-arm64-arm-64bit-Mach-O`; native backend `seatbelt`
- Runner: `python3 scripts/qualify_artifact.py --target aarch64-apple-darwin
  --revision <rev> --archive <archive> --evidence target/qualification/local`
- Acceptance set: `cargo fmt --all --check`, `cargo clippy --locked --all-targets
  --all-features -- -D warnings`, `cargo test --locked`, warning-free
  `cargo build --release --locked`

The runner extracted the archive into a private temporary directory, hashed both
the archive and the extracted binary against the packed `.sha256` /
`.binary-sha256` sidecars, executed the extracted binary for the surface and eval
checks, and re-hashed archive and binary after the last check. `cargo run` is
never used and no rebuilt executable is substituted; the extracted binary is
addressed through `BONSAI_SURFACE_BINARY`.

## Per-target results

| Target | Surface (artifact) | Inline (`--bin bonsai`) | Sandbox probes | release-eval | continuity-eval | Status |
| --- | --- | --- | --- | --- | --- | --- |
| aarch64-apple-darwin | 10 passed | 4,229 passed | 24 passed | pass | pass | **qualified** |
| x86_64-apple-darwin | — | — | — | — | — | unqualified (no native runner) |
| x86_64-unknown-linux-gnu | — | — | — | — | — | unqualified (no native runner) |
| aarch64-unknown-linux-gnu | — | — | — | — | — | unqualified (no native runner) |

Absent rows are reported as unqualified, not as passes. Each target must produce
its own `qualification.json`; the release matrix keeps `publish` dependent on a
successful `build` per target, and qualification runs inside that job after
**Pack binary** and before artifact upload, so an unqualified target stops the
release.

## Coverage map

- `artifact-surface` — `tests/surface_smoke.rs` executed against the extracted
  binary: successful tool execution and completion contract; denied
  approval-required mutation in noninteractive mode; project confinement of
  outside writes and symlink escapes; untrusted tool-result framing; malformed
  and truncated SSE without false success; bounded tool-loop exhaustion;
  strict binary-override rejection; production startup against a supported older
  store with sentinel preservation and against unsupported/corrupt stores with
  the original bytes preserved.
- `artifact_confines_native_writes_and_network` — the artifact itself (not the
  test-compiled in-process wrappers) must confine an out-of-project write, a
  symlink escape, a child-process write and loopback network egress, each with an
  unsandboxed positive control proving the target path/listener is reachable
  without confinement. The forbidden sentinel lives outside every sandbox
  writable root (project, private temp, OS temp).
- `inline` — full non-ignored inline suite for `--bin bonsai`: `src/redact.rs`,
  credential persistence in `src/storage/tests.rs`,
  `src/agent/tests/{web_injection,mcp_injection}.rs`, permissions/effect gates,
  `src/tool/secure_fs.rs` and path evidence, `src/provider/sse.rs`, and the
  cancellation/resume regressions.
- `sandbox-probes` — native enforcement of the real backend: out-of-root,
  symlink, child-process denial, confined allow-path, and deterministic
  loopback-listener network denial with an unsandboxed positive control. Run
  with `BONSAI_REQUIRE_NATIVE_SANDBOX=1`, where a missing or skipped backend is a
  failure rather than a pass. The required probe names exist in **both** the
  Linux `bubblewrap` and the macOS `seatbelt` module, and only the running
  target's module is compiled — so a target that cannot execute them reports the
  names as missing and fails, rather than inheriting the other platform's pass.
- `release-eval` / `continuity-eval` — the extracted binary runs
  `eval --mode mock --suite eval/suites/release_gating.toml --baseline
  eval/baselines/release-v1.toml --fail-on-task-failure` and
  `intent_continuity.toml --fail-on-task-failure` (cancellation, resumption,
  permission denial, shared-workspace persistence).
- The existing `--qualification` language/model matrix and the benchmark/soak
  validators are unchanged and are not part of this gate.

## Qualification findings and repairs

All findings below were found while building the gate and are repaired with
regressions; none is an unrepaired P0/P1.

1. **P1 — artifact identity could pass without native enforcement.** The first
   gate revision ran native confinement only through the source-level wrappers
   in `src/sandbox/tests.rs`, so a release-only regression that stopped wiring
   the sandbox into the shipped binary would have qualified. Repaired by adding
   `artifact_confines_native_writes_and_network`, which drives the *extracted*
   binary, and by making the runner require that named test to appear as executed
   (`missing_required_tests`), replacing the earlier count-only check.
2. **P1 — Darwin archives carried an extra member.** macOS `bsdtar` packs an
   AppleDouble `._bonsai` companion, so strict single-member validation rejected
   real Darwin release archives. Repaired by accepting exactly that companion
   name, never extracting it, and rejecting any other name, path or link type
   (regression: `test_archive_rejects_links_and_traversal`,
   `test_archive_accepts_macos_metadata_companion`).
3. **P2 — interleaved child output broke provenance parsing.** `curl` progress
   output split a test's `... ok` status line, so a required native test looked
   absent. Repaired by silencing the probe (`curl -s`) and by parsing test names
   independently of the trailing status token.
4. **P2 — silence could have read as success.** A required check that ran zero
   tests, printed a Bubblewrap skip marker, or omitted a required test name now
   fails the check instead of passing it (`test_missing_or_skipped_native_results_fail`,
   `test_missing_required_native_tests_fail`).

## Negative controls

| Control | Expectation | Observed |
| --- | --- | --- |
| Stub non-functional artifact (`exit 3`) through the real runner | runner fails, exit 1 | exit 1; `artifact-surface`, `release-eval`, `continuity-eval` failed |
| Archive/binary identity mismatch | runner fails, no checks run | `failure_reason` = `<archive\|binary> identity mismatch`, exit 1 |
| Missing required native test name / skipped backend / zero tests | check fails | status `failed`, `missing_required_tests` populated |
| Child timeout | check killed and reaped, group signalled | `reason` = `timeout`, status `failed` |
| Invalid or missing `BONSAI_SURFACE_BINARY` override | surface test fails, no fallback to the rebuilt binary | `invalid_surface_binary_override_never_falls_back` passed |
| Uploaded evidence | only `qualification.json` | fixed counters, hashes, revision, toolchain; no logs, databases, transcripts or environment dumps |

Synthetic credential-shaped values used by the fixtures never reach evidence: the
fixtures pass a clearly-fake key through an isolated child environment, and
`test_identity_failures_and_evidence_allowlist` packs a credential-shaped payload
into a candidate archive and asserts the report contains neither the payload nor
any log, and that `qualification.json` is the only retained file.

## Residual risk

- Three targets have no native evidence yet. The manual **Qualify native
  candidate** workflow runs the identical matrix and runner without publishing,
  and is the intended way to produce that evidence.
- The Linux qualification path depends on a working Bubblewrap on the runner
  (installed by the workflow). If runner policy prevents the native backend from
  enforcing, the gate fails rather than skipping; that is a runner qualification
  failure, not a pass.
- Cross-target qualification of the shipped archives is still to be observed in
  CI; a local aarch64-Darwin pass is recorded as exactly one target.

## Local artifacts

| Artifact | Local path | SHA-256 |
| --- | --- | --- |
| Qualification report (aarch64-apple-darwin) | `target/qualification/local/qualification.json` | — |
| Failure negative control | `target/qualification/fake/qualification.json` | — |
| Packed candidate | `dist/bonsai-local-aarch64-apple-darwin.tar.gz` | `cf53f71b117e53c75859d6ff4b18c2d3a8fba8e2750394d45c404a94dd157d8c` |

Raw per-check logs stay in the runner's private temporary directory and are
deleted with it; only the allowlisted report is retained or uploaded.
