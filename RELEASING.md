# Releasing Bonsai

This runbook covers the one-time public launch, release signing, release-candidate
publication, artifact verification, the website, Homebrew, and promotion to a stable
1.0 release. Bonsai is distributed through GitHub Releases and Homebrew. The crate has
`publish = false`, so crates.io is not part of the 1.0 release process.

## Release policy

- Use semantic-version tags prefixed with `v`, for example `v0.2.0-rc.1` or `v1.0.0`.
- Publish and qualify at least one release candidate before `v1.0.0`.
- Never reuse, move, or overwrite a published release tag.
- Do not use `--skip-checks` for an official build.
- Official artifacts come only from `.github/workflows/release.yml`; do not upload a
  locally compiled replacement under the same release tag.
- Keep self-update, Windows packages, containers, crates.io, npm, and PyPI outside the
  1.0 release unless their support is added deliberately.

## Prerequisites

The release operator needs:

- administrator access to `strozynskiw/bonsai`;
- Git, a stable Rust toolchain, Python 3, OpenSSL, `curl`, and the GitHub CLI (`gh`);
- access to the offline release-key backup;
- access to all four supported qualification targets, directly or through CI:
  - Ubuntu 22.04 or newer on x86-64;
  - Ubuntu 22.04 or newer on arm64;
  - macOS 13 or newer on Apple Silicon;
  - macOS 13 or newer on Intel.

The release workflow does not require Docker. External SWE-bench and Terminal-Bench
qualification belongs on a separately provisioned remote runner.

## One-time public-launch procedure

Complete this section before pushing the first public release tag.

### 1. Freeze and review the public tree

1. Finish, commit, and push every change intended for the public repository.
2. Remove abandoned branches, tags, releases, workflow artifacts, and generated files.
3. Review the full tree and history for credentials, private URLs, customer data,
   transcripts, personal information, and proprietary material.
4. Revoke and rotate every credential that has ever been committed or printed in an
   Actions log. Rewriting history is not a substitute for rotation.
5. Update `README.md`, `site/index.html`, package metadata, screenshots, installation
   examples, provider counts, support claims, and the roadmap to match the shipped build.
6. Ensure public installation examples use HTTPS Git URLs rather than SSH URLs.
7. Review `LICENSE` and `SECURITY.md`.

GitHub makes repository contents and existing Actions history/logs public when visibility
changes. It also disables push rulesets during a private-to-public conversion. Review the
[visibility-change consequences](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/managing-repository-settings/setting-repository-visibility)
before continuing.

### 2. Reset private history, if still required

`ROADMAP.md` currently calls for a clean public history. If that decision still stands:

1. Create an encrypted, access-controlled backup of the private repository history.
2. Produce a reviewed orphan root commit containing only the intended public tree.
3. Replace the remote default branch with that reviewed history.
4. Delete every obsolete remote branch and tag.
5. Delete old Actions runs and artifacts that must not become public.
6. Verify the repository from a fresh unauthenticated clone before changing visibility.

This operation is destructive and should be executed from a dedicated backup clone. Do
not improvise it in the normal working copy.

### 3. Create the production Ed25519 signing key

Generate the key outside the repository:

```sh
umask 077
mkdir -p "$HOME/.local/share/bonsai-release"

openssl genpkey \
  -algorithm ED25519 \
  -out "$HOME/.local/share/bonsai-release/private.pem"

openssl pkey \
  -in "$HOME/.local/share/bonsai-release/private.pem" \
  -pubout \
  -out "$HOME/.local/share/bonsai-release/public.pem"

openssl pkey \
  -in "$HOME/.local/share/bonsai-release/private.pem" \
  -pubout -outform DER |
  tail -c 32 |
  openssl base64 -A
printf '\n'
```

Store the complete private PEM as the repository Actions secret
`BONSAI_RELEASE_PRIVATE_KEY`. Store the final one-line Base64 value as the repository
Actions variable `BONSAI_RELEASE_PUBLIC_KEY`:

- <https://github.com/strozynskiw/bonsai/settings/secrets/actions>

Keep an additional encrypted/offline copy of the private key. GitHub secrets cannot be
downloaded later. Never commit either PEM file. Existing official binaries embed this
public key, so key loss or unplanned rotation breaks their release-verification chain.

### 4. Make the repository public

1. Open <https://github.com/strozynskiw/bonsai/settings>.
2. Go to **General → Danger Zone → Change repository visibility**.
3. Select **Public** and confirm `strozynskiw/bonsai`.
4. Verify these URLs without an authenticated browser session:
   - <https://github.com/strozynskiw/bonsai>
   - <https://raw.githubusercontent.com/strozynskiw/bonsai/master/install.sh>

The repository must be public before the release tag is pushed. On non-Enterprise GitHub
plans, the release workflow's provenance and SBOM attestations are available for public
repositories and are intentionally skipped while the repository is private.

### 5. Restore and enable repository protections

Immediately after the visibility change:

1. In **Settings → Rules**, protect `master` from force-push and deletion and require the
   Rust CI checks before merge.
2. Add a tag rule protecting `v*` from modification and deletion.
3. In **Settings → Actions → General**, keep the default `GITHUB_TOKEN` permissions
   read-only. The release workflow grants narrowly scoped write permissions only to its
   publishing job. Do not allow Actions to approve pull requests.
4. Ensure the repository permits GitHub-owned actions plus the pinned
   `anchore/sbom-action` and `rustsec/audit-check` actions used by the workflows.
5. In **Settings → Advanced Security**, enable or confirm:
   - dependency graph;
   - Dependabot alerts and security updates;
   - secret scanning and push protection;
   - private vulnerability reporting.
6. Subscribe the maintainers to security-alert notifications.
7. Configure the repository description, website URL, topics, social preview, and issue
   tracker. Keep Discussions optional and disable the wiki unless it will be maintained.

Private vulnerability reporting is configured under
<https://github.com/strozynskiw/bonsai/settings/security_analysis>. Confirm that the
**Report a vulnerability** button appears on the public Security page.

## Preparing every release

### 1. Choose the version and release kind

Examples:

- release candidate: `0.2.0-rc.1`;
- later release candidate: `0.2.0-rc.2`;
- stable release: `1.0.0`.

Release candidates remain GitHub prereleases. Before a stable release, change the release
workflow so `v1.0.0` is not created with the hard-coded `--prerelease` flag. Commit and
qualify that workflow change before creating the stable tag.

### 2. Check public documentation and version references

Before invoking the release script:

```sh
rg -n 'v[0-9]+\.[0-9]+\.[0-9]+|alpha|release candidate|prerelease' \
  README.md site/index.html Cargo.toml scripts/release.sh
```

Correct stale installation commands, release labels, and support claims in a normal commit.
`scripts/release.sh` expects to replace the current Cargo version tag in the README,
website, and installer help. If any public surface does not contain that tag, fix the
stale reference first; otherwise the script stops without creating a commit or tag.

### 3. Require a clean, synchronized branch

```sh
git switch master
git fetch origin
git status --short
git log --oneline --decorate -5
```

The worktree and index must be empty, `master` must contain every intended release change,
and the local branch must match the reviewed remote commit. Do not release from a worktree
shared with unfinished agent changes.

### 4. Run the release gates

Run the same core gates as CI:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
RUSTFLAGS=-D warnings cargo build --release --locked
cargo run --locked -- eval \
  --mode mock \
  --suite eval/suites/release_gating.toml \
  --baseline eval/baselines/release-v1.toml \
  --fail-on-task-failure
cargo run --locked -- eval \
  --mode mock \
  --suite eval/suites/language_acceptance.toml \
  --baseline eval/baselines/release-v1.toml \
  --fail-on-task-failure
```

Before 1.0, also confirm that the remote external benchmark qualification, adversarial
safety suites, migration/upgrade fixtures, and the two-week RC soak meet the frozen release
thresholds in `ROADMAP.md`.

### 5. Prepare the release commit and tag locally

Use `--no-push` so the generated commit and tag can be reviewed:

```sh
scripts/release.sh 0.2.0-rc.1 --no-push
```

The script updates `Cargo.toml`, `Cargo.lock`, and public install references; runs its
release checks; creates `chore(release): prepare v<version>`; and creates an annotated tag.

Review the result:

```sh
git status
git show --stat --decorate HEAD
git show --stat "v0.2.0-rc.1"
```

Confirm that:

- the tag points to the release commit;
- the package and displayed version are correct;
- the release workflow and signing-key references exist in that commit;
- no unrelated work entered the release commit.

### 6. Push the commit, then the tag

```sh
git push origin master
git push origin v0.2.0-rc.1
```

Pushing `master` first ensures the public branch contains the tagged release commit before
the tag-triggered workflow publishes assets. Never push the tag if the branch push fails.

## Monitoring the release workflow

Open <https://github.com/strozynskiw/bonsai/actions/workflows/release.yml> and require every
job to pass:

1. **Verify release** — formatting, manifest tests, clippy, tests, deterministic evals,
   and install smoke test.
2. **Build** — all four supported Rust targets compile, smoke-test, and emit a native
   release-binary performance report. A reviewed target baseline turns that report into a
   material-regression gate; a target without one remains explicitly report-only.
3. **Generate release SBOM** — a pinned Syft version creates the SPDX JSON document in a
   separate read-only job.
4. **Publish release** — archives and checksums are collected, the canonical manifest is
   signed, provenance/SBOM attestations are created, and the GitHub release is published.

The release must contain:

- `bonsai-<tag>-x86_64-unknown-linux-gnu.tar.gz`;
- `bonsai-<tag>-aarch64-unknown-linux-gnu.tar.gz`;
- `bonsai-<tag>-x86_64-apple-darwin.tar.gz`;
- `bonsai-<tag>-aarch64-apple-darwin.tar.gz`;
- one `.sha256` file per archive and completion script;
- `SHA256SUMS`;
- `release-manifest.json` and `release-manifest.json.sig`;
- one `bonsai-<tag>-<target>.performance.json` and checksum per target;
- `bonsai-<tag>.spdx.json`;
- `bonsai.bash`, `_bonsai`, and `bonsai.fish`.

If any job fails, do not move the tag. Fix the cause, increment the prerelease version, and
publish a new tag. A failed unpublished tag may be deleted only after confirming no release
or artifact escaped; a published tag is immutable.

## Verifying a published release

Perform the following drill on every supported target. Replace the example values as
needed:

```sh
TAG=v0.2.0-rc.1
TARGET=aarch64-apple-darwin
ARCHIVE="bonsai-$TAG-$TARGET.tar.gz"

mkdir -p "/tmp/bonsai-$TAG-verification"
cd "/tmp/bonsai-$TAG-verification"

gh release download "$TAG" --repo strozynskiw/bonsai
shasum -a 256 -c SHA256SUMS
gh attestation verify "$ARCHIVE" --repo strozynskiw/bonsai
```

Install the exact tag through the public installer:

```sh
curl -fsSL \
  https://raw.githubusercontent.com/strozynskiw/bonsai/master/install.sh |
  BONSAI_VERSION="$TAG" sh
```

Then run:

```sh
bonsai --version
bonsai --help
bonsai doctor
bonsai doctor --online
```

The online doctor must verify the running executable against the signed manifest before it
makes a version or update claim. Also authorize a provider, select a model, trust a test
workspace, and complete one representative inspect/edit/verify/review task.

Record the tag, target, operating-system version, archive hash, attestation result, doctor
result, installation result, task result, and tester. Keep credentials and transcripts out
of the qualification record.

## Establishing performance baselines and soaking the RC

Before the final RC, run `.github/workflows/performance.yml` on all four native runner
classes. Review and commit one matching file under `eval/baselines/performance/` per
target; never auto-accept a new reference from the change it is judging. The reports record
startup, idle CPU, time to first assistant output, final shared-snapshot persistence, RSS,
binary size, provider-reported context growth and cache reuse, and deterministic task cost.
Hosted-runner timing and memory gates require both a relative regression and a meaningful
absolute increase; token, cache, binary-size, and cost gates are tighter.

Publish a new RC after all four reviewed baselines exist. Its release workflow must emit
four reports with `baseline.passed: true`, and each report's `identity.binary_sha256` must
match the corresponding binary hash in the signed manifest. Download the exact assets,
verify their checksums, signature, and attestations, then create
`release/soak/active/rc-<tag>.json` from the inert template. Pin the peeled tag commit,
manifest hash, all binary hashes, and all performance-report hashes before recording the
UTC start time. Do not backdate the start.

Dogfood those unchanged hashes for at least 336 elapsed hours. Append secret-free passing
observations on at least 14 distinct UTC dates, exercising both TUI and headless work; keep
prompts, transcripts, code, credentials, environment values, and local paths out of the
record. Record every data-loss, security, migration, or task-completion incident by stable
ID and classification. Any release-blocking incident permanently disqualifies that RC,
even if fixed later: publish a new immutable candidate and restart the clock.

The daily `RC Soak` workflow validates active records against freshly downloaded release
assets, their provenance, record history, and the local peeled tag. Before promotion,
dispatch it with `require_qualified` enabled, or run the equivalent local command documented
in `release/soak/README.md`. A unit test may inject time into validator functions, but the
CLI and workflow use real UTC plus the GitHub release publication time. Protected,
reviewed, append-only branch history remains the trust boundary for observation dates.

## Publishing the website

Repository visibility exposes the source under `site/`; it does not deploy a website.
GitHub Pages branch publishing accepts the repository root or `/docs`, not `/site`.
Therefore either:

- add a GitHub Pages Actions workflow that deploys `site/` (preferred); or
- move the website to `docs/` and publish it from `master`.

Configure the source at <https://github.com/strozynskiw/bonsai/settings/pages>. Verify the
default Pages URL before configuring a custom domain. Confirm that every installation,
release, documentation, and GitHub link works from an unauthenticated browser.

## Publishing and updating Homebrew

After the first signed RC passes the four-target drill, create the public repository
`strozynskiw/homebrew-tap`. Keep the Bonsai formula at `Formula/bonsai.rb`.

The formula must select the correct macOS architecture, use immutable GitHub release URLs,
pin the published SHA-256 values, install the `bonsai` binary, install completions where
appropriate, and test `bonsai --version`.

Test before publishing:

```sh
brew tap strozynskiw/tap
brew install strozynskiw/tap/bonsai
brew test strozynskiw/tap/bonsai
bonsai doctor
```

Document the public installation command:

```sh
brew install strozynskiw/tap/bonsai
```

For later releases, update the formula only after the GitHub assets and checksums have been
published and verified. Keep the old formula commit available through Git history so a bad
formula update can be reverted without changing a Bonsai release tag.

## Promoting 1.0

Create `v1.0.0` only after every item in the `ROADMAP.md` 1.0 release gate is satisfied:

- no open release-blocking security, authorization, migration, sandbox, or data-loss issue;
- frozen agent-quality and public benchmark thresholds pass;
- Rust, TypeScript/JavaScript, Python, and Go acceptance tasks pass;
- TUI and headless semantics agree;
- four-target build, install, and smoke qualification passes;
- adversarial suites pass;
- schema compatibility, backup, downgrade limitations, and recovery are tested and
  documented;
- public documentation matches the binary;
- the minimum two-week RC soak completes without a blocking incident.

Before tagging `v1.0.0`:

1. Remove or conditionally disable `--prerelease` for stable tags in the release workflow.
2. Update README and website language from alpha/RC to stable.
3. Confirm the Homebrew formula update is ready but do not publish it early.
4. Repeat the full release gates.
5. Prepare with `scripts/release.sh 1.0.0 --no-push` and review the commit and tag.
6. Push `master`, then `v1.0.0`.
7. Verify all four artifacts, attestations, the signed manifest, the installer, doctor,
   website, and Homebrew.
8. Mark the release as latest and update `ROADMAP.md` only after verification succeeds.

## Incident and rollback rules

- Never replace an archive or checksum under an existing release tag.
- For a packaging or documentation defect, publish a new patch or prerelease tag.
- For a security defect, disable or clearly mark the affected release, prepare the fix in
  private through a GitHub security advisory, rotate exposed credentials, and publish a new
  immutable version.
- If the signing key is suspected compromised, stop publishing immediately. Preserve the
  evidence, rotate the key through an explicitly designed trust-transition release, and do
  not silently overwrite `BONSAI_RELEASE_PUBLIC_KEY` while existing binaries trust the old
  key.
- If Homebrew is broken but GitHub artifacts are sound, revert only the tap formula.
- If the website is broken, roll back only the Pages deployment.
- Record the incident and remediation in the GitHub release notes or security advisory;
  do not alter historical artifacts.

## Release completion checklist

- [ ] Public tree and history reviewed; credentials rotated where necessary.
- [ ] Repository public and accessible without authentication.
- [ ] Branch and `v*` tag protections active.
- [ ] Private vulnerability reporting and security notifications active.
- [ ] Production signing secret and public variable configured and backed up.
- [ ] Clean `master`; local and CI release gates green.
- [ ] Release commit and annotated tag reviewed before push.
- [ ] All four archives, checksums, completions, SBOM, and signed manifest published.
- [ ] Provenance and SBOM attestations verified.
- [ ] Installer and `bonsai doctor --online` verified on all four supported targets.
- [ ] Representative first-run task completed on every supported platform class.
- [ ] Four reviewed performance baselines pass against the exact published binaries.
- [ ] Immutable RC soak record validates as `qualified` after at least 336 real hours.
- [ ] Website deployed and checked anonymously.
- [ ] Homebrew formula installed and tested.
- [ ] Release notes and roadmap status updated.
- [ ] For 1.0, the complete release gate and two-week RC soak are documented as passed.
