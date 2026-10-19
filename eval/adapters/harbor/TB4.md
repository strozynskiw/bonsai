# TB4 CPU-only comparison: offline preparation and readiness

**Status: pinned Harbor adapters, local-task backend, approval-bound live CLI,
and external billing-file integration are implemented and fixture-tested.**
No real model comparison, benchmark score, successful benchmark smoke, or Linux
runner qualification is claimed. The live backend rejects macOS/Docker Desktop;
Linux isolation and request evidence plus explicit spending approval are required.

## Locked identities

- Dataset: `terminal-bench/terminal-bench@4.0.0`, plus operator-measured content
  digests for the dataset and each task. CPU selection is not the full TB4.
- Harbor: `7331d8a3e23cb72e8277522059ff7c9ab9435d90`.
- DSH: clean `687f282e8625c6b8d11ff58211a90524be2e4317`.
- Record Bonsai source revision, dirty status, source-content and executable
  digests; DSH source/package digest; Node version/executable digest; dependencies
  digest; both profile overlay digests; and immutable prepared image digests.
- Historical TB2 and SWE-bench contracts/imports are separate. TB4 qualification
  imports accept only Bonsai, not DSH comparison records.

Sources: [official TB4 execution](https://www.tbench.ai/run),
[locked Harbor lifecycle](https://github.com/harbor-framework/harbor/blob/7331d8a3e23cb72e8277522059ff7c9ab9435d90/src/harbor/trial/trial.py),
[locked DSH headless profile](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e8625c6b8d11ff58211a90524be2e4317/packages/bundle/headless/cordis.patch.yml).

## Explicit Linux setup boundary

Do not run setup as part of preparation, tests, reports, or imports. On a
separately approved Linux runner, the operator must explicitly obtain the pinned
sources, build both artifacts, prepare task images, and create a reviewed
hash-locked transitive Python wheelhouse. `tb4-lock.txt` pins and hashes the 90
third-party packages observed in the Python 3.14 local fixture runtime; requalify
that snapshot on Linux. `tb4-requirements.txt` separately pins Harbor's Git source
because VCS requirements cannot use `--require-hashes`. Install dependencies with
`uv pip install --python "$PY" --require-hashes -r eval/adapters/harbor/tb4-lock.txt`,
then Harbor with `uv pip install --python "$PY" --no-deps -r eval/adapters/harbor/tb4-requirements.txt`.
These are explicit online setup commands, not part of tests or execution. For
an air-gapped runner prepare/review a wheelhouse and add `--no-index --find-links`;
build/hash Harbor's wheel separately and preserve its source provenance. The
current runtime validator expects Harbor's VCS `direct_url.json` commit, so a
wheel-only deployment needs a separately reviewed provenance path first.
No setup/download command is executed by the comparison driver.

Use identical prepared task images for both agents, prerequisites outside task
files, no baked keys, no host-home or Docker-socket mounts, and fresh homes and
sessions for every attempt. Keep reference solutions/verifiers outside normal
agent access. Harbor's own artifact/log mounts must be reviewed too; saying
"no user mounts" is not proof of isolation. Validate verifier environment mode,
network enforcement and descendant cleanup on the actual runner. Do not silently
disable a missing sandbox backend. Explicitly approve container-only execution
if selecting Bonsai `--isolation off` and DSH `danger-full-access`.

Adapters do not install either CLI. Bonsai requires `b3sum` for its historical
qualification metadata. The DSH adapter expects `/opt/benchmark/bin/dsh`, the
prebuilt workspace `/opt/benchmark/dsh`, Node on the approved path, and
`dsh_export.mjs` installed at `/opt/benchmark/dsh_export.mjs`. Its exporter uses
the pinned public persistence API, decodes compressed versioned logs, follows
child lineage, and excludes inherited fork prefixes from usage sums.

## Local inputs and offline commands

Run commands from the repository root using a project virtual environment, not
system Python. Set `PY` to its interpreter. Keep inputs/results under the ignored
`target/tb4-comparison/` directory; private traces should be mode 0600 in a mode
0700 directory. Do not commit task solutions or upload any artifact automatically.

Supply `settings.json` with the fields validated by
`comparison_manifest.validate_manifest`: schema version, exact pins, content
identities, `node`, `artifacts`, `profiles`, `model`, and `resources`.
`test_comparison_manifest.manifest_fixture()` documents the JSON shape with
**synthetic values that must not be used as execution provenance**. The common
model requires an exact remote ID, credential-free HTTPS base endpoint,
`off|low|high|max`, a disclosed alias limitation, and `fallback: false`.
Resource CPU/RAM/disk/timeout numbers have no invented production defaults.

Export/review the complete authoritative task resource inventory locally as:

```json
{
  "dataset": "terminal-bench/terminal-bench@4.0.0",
  "dataset_digest": "sha256:<measured dataset identity>",
  "tasks": [{
    "id": "<task id>",
    "content_digest": "sha256:<measured content identity>",
    "image_digest": "sha256:<prepared immutable image identity>",
    "resource_source": "<task id>/task.toml",
    "requirements": {
      "gpus": 0, "cpu": 1, "ram_mb": 1024, "disk_mb": 1024,
      "unsupported": []
    }
  }]
}
```

The numbers above describe shape only: use actual authoritative requirements.
Unknown resource requirements are exclusions, not implicit CPU eligibility.
Selection hashes `seed + NUL + task-id`, sorts by that hash, and freezes 3 smoke
plus 10 distinct pilot tasks. Fewer than 13 eligible tasks fails preparation.
Three scored repetitions yield 60 trials; smoke yields 6 unscored trials.

```sh
"$PY" -m eval.adapters.harbor.comparison prepare \
  --settings target/tb4-comparison/settings.json \
  --inventory target/tb4-comparison/inventory.json --seed '<chosen seed>' \
  --out target/tb4-comparison/manifest.json
"$PY" -m eval.adapters.harbor.comparison check \
  --manifest target/tb4-comparison/manifest.json
"$PY" -m eval.adapters.harbor.comparison dry-run \
  --manifest target/tb4-comparison/manifest.json \
  --records target/tb4-comparison/smoke-records.json --phase smoke
"$PY" -m eval.adapters.harbor.comparison dry-run \
  --manifest target/tb4-comparison/manifest.json \
  --records target/tb4-comparison/pilot-records.json --phase pilot
"$PY" -m eval.adapters.harbor.comparison report \
  --manifest target/tb4-comparison/manifest.json \
  --records target/tb4-comparison/pilot-records.json \
  --out target/tb4-comparison/report-001
```

Records files are explicit JSON arrays (`[]` before any attempts). The offline
commands above never fetch tasks/images, launch a runtime, or call a provider. Output paths must
be new; existing manifests/reports are not overwritten. Dry-run with existing
records is the offline resume preview: it skips every occupied identity,
including failures/invalid attempts. No best-of-three and no invisible reruns.
Infrastructure-invalid retries require a separately reviewed experiment while
retaining originals. `FileJournal` provides exclusive locking and fsynced
pre-effect reservations; interrupted reservations remain occupied.

Reports accept only normalized public fields (see `records_fixture` in
`test_comparison_report.py`), retain missing usage/cost as null, keep billed,
estimated and infrastructure cost separate, and leave cost per success undefined
when no successes occur. Bootstrap intervals resample complete task clusters,
with a manifest-seeded deterministic PRNG. Small-pilot medians/tails are
descriptive, not precise speed claims. Only relative `redacted/` log links are
accepted; redaction/review of those files remains an operator responsibility.

## Live approval and execution

The following command dispatches only after all approval, manifest, Linux runner,
billing, and preflight checks succeed. Do not run it with synthetic provenance.
Set `TB4_PROVIDER_API_KEY` out of band; never put a key in the manifest or evidence.

```sh
"$PY" -m eval.adapters.harbor.comparison live --execute \
  --manifest target/tb4-comparison/manifest.json --phase smoke \
  --records target/tb4-comparison/smoke-records.json \
  --approval target/tb4-comparison/approval.json \
  --task-root '<reviewed local tasks>' --journal target/tb4-comparison/smoke-journal \
  --raw-output target/tb4-comparison/private-smoke \
  --billing target/tb4-comparison/billing.json --evidence target/tb4-comparison/evidence.json
```

Approval validation requires exact manifest/phase, a named Linux runner and
human approver, positive provider/infrastructure limits and per-trial
reservation, external hard-cap evidence digest, isolation evidence digest,
and explicit external-cap/network/nested-confinement approval. A local estimate
is not a billing cap. Missing/stale billing observations or an exhausted budget
stop scheduling. No question timeout grants approval.

Pilot approval additionally requires `smoke_evidence_digest`, computed by
`comparison_gate.smoke_digest(manifest, records)` from all six successful smoke
records sorted by identity. Supply their retained JSON file as `--smoke-records`
for pilot commands. Approval parsing and runner preflight both reject missing,
duplicate, failed, changed, or model/artifact/profile/manifest-mismatched smoke
evidence. Freeze the configuration before smoke; any configuration change
requires new smoke evidence, not rebinding old outcomes to a new manifest.

Journal reservations are built in private `.pending-` staging directories and
published atomically only after the complete reservation is fsynced. An orphaned
staging directory cannot precede a dispatched trial and is ignored on resume;
a published reservation is always occupied, even without a result.

The narrow backend supports only frozen local single-step tasks with immutable
preinstalled images, no custom Compose/MCP/skills, a provider-host-only allowlist,
and a separate verifier environment. Unsupported tasks fail closed. Do not edit
benchmark tasks after selection: changed task/image bytes require new provenance.
Bonsai uses `anthropic-compatible` to match DSH's native Messages transport;
the native Bonsai DeepSeek Chat connector is deliberately rejected here.
Actual outgoing model/thinking and returned-model identity still require reviewed
request evidence from the intended endpoint before benchmark execution.

The optional digest-covered manifest `execution` object contains `task_paths`,
`image_ids`, `artifact_paths` (absolute in-container paths for both agents),
`profiles`, `request_evidence_digest`, `external_cap_evidence_digest`, and
`disk_enforcement: "dedicated-runner-volume"`. Each native profile is an object
with `options` and `environment`; its digest must equal that agent's
`profiles.<agent>.overlay_digest`. Bonsai's sole environment binding is
`BONSAI_PROVIDER: "anthropic-compatible"`; DSH's is empty. Bonsai options must
supply every explicit adapter limit and the common reasoning/timeout/network.
DSH uses its recorded native defaults. Tasks are bound by `local_trial.tree_digest`,
which includes relative filenames, executable bits and SHA256 contents and rejects
links/special files. Image IDs are checked before and during execution; artifact
SHA256 and Node SHA256 are checked before version/setup or credential release.

`evidence.json` maps `request_evidence_digest`, `external_cap_evidence_digest`, and
`isolation_evidence_digest` to their reviewed JSON bodies (not digest strings).
Request evidence binds the exact `model` object, `protocol: "messages"`,
`both_agents_observed: true`, and `fallback_disabled: true`. Isolation evidence
binds `platform: "linux"`, the exact approved `runner` hostname, manifest `resources`
and `image_ids`, and explicit true observations for `no_host_credentials`,
`no_host_home_or_socket`, `network_enforced`, `disk_quota_enforced`,
`verifier_isolated`, `cancellation_teardown_verified`. These are operator evidence
contracts, not a substitute for actual runner measurements.

An independent observer atomically replaces private (0600) `billing.json` with
exact fields: `manifest_digest`, `phase`, nonnegative increasing `sequence`,
timezone-aware ISO `observed_at`, `settled_identity` (null until settled), decimal
`provider_usd`, decimal `infrastructure_usd`, `external_cap_evidence_digest`.
Totals are cumulative for that manifest/phase and include every child/retry.
Snapshots older than 30 seconds, regression, changed totals under the same sequence,
or missing per-attempt settlement stop execution. The observer and provider hard
cap are external operator infrastructure; this repository supplies the validated
reader, not a provider-specific billing API collector. Budget checks repeat before
setup and immediately before scoped credential release. Settlement is read only
after teardown. Unknown cost is never converted to zero.

Then pass all 3 unscored smoke tasks for both agents under the frozen
configuration, retain their evidence, and obtain new explicit pilot approval before
60 scored attempts. Use the same command with `--phase pilot`, its separate
journal/approval/billing phase and `--smoke-records`. Resume repeats the command
with an exported exact journal record array as `--records`; occupied identities
are never rerun. Read records via `FileJournal.records()` while holding its lock.
Missing settlement on a previous attempt prevents scheduling more trials.
Bypassing guards or calling an adapter directly is not a qualification workaround.

## Offline verification lanes

```sh
"$PY" -m unittest discover -s eval/adapters/harbor -t . -v
"$PY" -m unittest eval.adapters.phase_e.test_import -v
"$PY" eval/adapters/phase_e/run_canary.py selftest
"$PY" -m compileall -q eval/adapters/harbor
node eval/adapters/harbor/test_dsh_export.mjs '<prebuilt pinned DSH workspace>'
cargo fmt --all -- --check
cargo test --locked eval::adapters::
cargo test --locked
cargo clippy --all-targets --all-features -- -D warnings
RUSTFLAGS='-D warnings' cargo build --release --locked
```

The installed-agent lifecycle tests skip without Harbor; they do not install it.
Run them again under the reviewed pinned environment and treat a skip as missing
evidence, not a pass. The explicit DSH export lane uses only a prebuilt local
workspace and synthetic compressed parent/child logs, not inference.

### Synthetic container lane and current evidence

Pinned Harbor is installed locally in `target/tb4-harbor-venv`. The five real
base-class fixtures pass without skips. The synthetic container lane uses fake
CLIs, a fake DSH persistence exporter and fake b3sum; **none are valid benchmark
artifacts**. It proves Harbor reward integration, CLI failure, timeout, cancellation,
setup rejection, credential staging and container teardown—not model quality,
Linux allowlist enforcement or separate-verifier isolation.

Explicitly prepare an image from a locally present Bash-capable base (no pulls):

```sh
docker image inspect '<local base tag>'
docker tag '<reviewed local image ID>' bonsai-tb4-base:local
docker build --pull=false --network=none -t bonsai-tb4-synthetic:local \
  eval/adapters/harbor/fixtures/cli
TB4_FIXTURE_IMAGE=bonsai-tb4-synthetic:local \
  TB4_DSH_RUNTIME=target/comparison/deepseek-harness-687f282e86 "$PY" -m unittest discover \
  -s eval/adapters/harbor -t . -v
```

Run this lane again on the approved Linux host. Its synthetic custom Compose uses
`network_mode: none`, which is distinct from the live backend's Linux-only
allowlist policy. Default discovery skips Docker tests unless
`TB4_FIXTURE_IMAGE` is explicitly set; skips are missing evidence, not passes.
The installed-Harbor/container/pinned-CLI suite passed 43 tests with no skips,
including the setup-boundary and fresh-session regressions. The real pinned DSH
CLI is exercised without credentials against a loopback endpoint; its emitted
fresh identity is checked and its compressed durable failure is exported.
Synthetic DSH now rejects `--session-id`: that flag resumes an existing session,
not a new identity. The adapter launches without it, reads exactly one safe
`session` event, and requires the durable root outcome before accepting success.
Security review corrections ensure setup has no API key, artifact/confinement
checks precede version invocation, the approved runner matches the actual host,
and fresh billing reruns the budget decision. No paid inference or benchmark
execution was performed by this integration verification. A separate native-CLI
live smoke is recorded in `eval/evidence/deepseek-flash-paired-smoke.md`; it is
not Harbor/TB4 qualification. Remaining operational blockers are real Linux isolation
qualification, request evidence, an external observer/hard cap and human approval.
