# Release-candidate soak records

An RC qualifies only when the same published binaries have run unchanged for at
least 336 hours, have passing observations on 14 distinct effective UTC days,
cover both `headless` and `tui`, and have no release-blocking data-loss,
security, migration, or task-completion incident anywhere in the record's Git
history. The validator also requires passing, internally consistent performance
reports for every release target.

## Start a soak

Copy `rc-soak.template.json` to
`release/soak/active/rc-vMAJOR.MINOR.PATCH-rc.N.json`, then replace every inert
value with evidence from the published prerelease. The release identity must
pin:

- the strict semver RC tag and its peeled commit;
- the SHA-256 of the exact signed `release-manifest.json` bytes;
- the binary SHA-256 and performance-report SHA-256 for all four targets;
- the claimed UTC start and a minimum of 336 hours.

The exact target set is `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, and
`x86_64-apple-darwin`. Commit the new active record before validating it. An
active record must be tracked at `HEAD`, match `HEAD` byte-for-byte, and have
one creation commit.

The effective start is the latest of the claimed start, the record's first Git
commit, and the GitHub release `publishedAt` time. Under protected, reviewed
history, this prevents ordinary backfill before the evidence or immutable
release existed. Git committer timestamps are operator evidence, not
cryptographic server time: protect the active-record path and require review,
because a hostile history author can synthesize them.

Append observations in a new commit on the day they occur. Observations and
incidents are strict append-only arrays; release identity and soak settings
never change. An observation's effective UTC day is the latest of its claimed
timestamp, its first-seen commit time, and the effective soak start. A claimed
observation committed more than 36 hours later is rejected, so ordinary
backfill of 14 old rows in one reviewed commit cannot qualify an RC. Resolving
or later deleting a release-blocking incident never restores qualification.

Records contain identifiers, timestamps, enum values, and hashes only. Do not
put prompts, transcripts, secrets, free-form descriptions, machine paths, or
other user data in them. Evidence file paths are CLI arguments and are never
persisted in the record.

## Validate

Use a full Git checkout and the exact assets downloaded from the GitHub
prerelease:

```sh
python3 scripts/check-rc-soak.py \
  --record release/soak/active/rc-v1.2.3-rc.1.json \
  --manifest target/soak/release-manifest.json \
  --manifest-signature target/soak/release-manifest.json.sig \
  --release-public-key target/soak/release-public-key.pem \
  --release-published-at 2026-07-14T12:00:00Z \
  --archive x86_64-unknown-linux-gnu=target/soak/bonsai-v1.2.3-rc.1-x86_64-unknown-linux-gnu.tar.gz \
  --archive aarch64-unknown-linux-gnu=target/soak/bonsai-v1.2.3-rc.1-aarch64-unknown-linux-gnu.tar.gz \
  --archive aarch64-apple-darwin=target/soak/bonsai-v1.2.3-rc.1-aarch64-apple-darwin.tar.gz \
  --archive x86_64-apple-darwin=target/soak/bonsai-v1.2.3-rc.1-x86_64-apple-darwin.tar.gz \
  --performance-report x86_64-unknown-linux-gnu=target/soak/bonsai-v1.2.3-rc.1-x86_64-unknown-linux-gnu.performance.json \
  --performance-report aarch64-unknown-linux-gnu=target/soak/bonsai-v1.2.3-rc.1-aarch64-unknown-linux-gnu.performance.json \
  --performance-report aarch64-apple-darwin=target/soak/bonsai-v1.2.3-rc.1-aarch64-apple-darwin.performance.json \
  --performance-report x86_64-apple-darwin=target/soak/bonsai-v1.2.3-rc.1-x86_64-apple-darwin.performance.json
```

The signature must be base64-encoded Ed25519 over the exact manifest bytes;
the public key is PEM. Every `.tar.gz` is hashed, safely inspected without
extracting to disk, and required to contain exactly one executable regular
`bonsai` binary whose hash and size match the signed manifest and performance
report. The workflow also requires GitHub artifact attestations from
`.github/workflows/release.yml` at the pinned tag and commit.

The command prints one JSON object. Exit `0` means `qualified`, `2` means
`pending`, `3` means validation failed or a blocking incident exists, and `64`
means invalid CLI usage. There is intentionally no command-line clock override;
tests inject time only through the Python function.

The daily `RC soak qualification` workflow performs the same checks. A pending
scheduled soak is healthy; use its `require_qualified` dispatch input when a
release decision must fail until the full exit bar is met. The template itself
is inert and does not claim that any RC is active or qualified.
