# Remote qualification environment

Only on the explicitly approved Linux amd64 runner. No local/CI network installs.
Use Python 3.11 and record its exact patch version, pip/uv versions, OS image digest,
Docker version and b3sum version in private runner evidence.

## Resolve once, review, then freeze

The direct source revisions live in runner-requirements.txt. On a disposable remote
preparation host, after approval, resolve **once** with a fixed uv version:

```sh
uv pip compile eval/adapters/runner-requirements.txt --python-version 3.11 --generate-hashes --output-file runner.lock
python3.11 -m venv builder
builder/bin/python -m pip wheel --requirement runner.lock --wheel-dir wheelhouse
```

VCS source entries do not support pip --require-hashes. Build the two pinned VCS
packages into wheels in the preparation step; replace VCS entries in the deployment
lock with their wheel filenames and SHA256 hashes. Include every transitive wheel
with exact version and SHA256. Review direct_url.json in the built distributions,
record both source commits in installed-source.json, and sign/checksum the lock,
wheelhouse and provenance together. Archive them under an immutable runner release.
Do not resolve again for a candidate run.

## Deterministic deployment (offline)

```sh
python3.11 -m venv .venv
.venv/bin/python -m pip install --no-index --find-links wheelhouse --require-hashes -r deployment.lock
.venv/bin/python -m pip check
```

Preflight currently requires installed VCS direct_url.json metadata. If deploying
built wheels, retain that source metadata during wheel preparation; do not invent
it from the manifest. Verify wheel hashes before deployment. Reject missing or
conflicting installed-source records. The remote preparation procedure is not a
checked-in resolved lock: qualification is blocked until its reviewed artifacts
exist. Credentials never belong in the wheelhouse, image or manifest.

Offline developer checks use a clean stdlib-only venv; they do not run the above
commands or install Harbor/SWE dependencies. The end-to-end synthetic test is:

```sh
cargo test --locked offline_harness_bundle
.venv/bin/python -m unittest eval.adapters.phase_e.test_import
```
