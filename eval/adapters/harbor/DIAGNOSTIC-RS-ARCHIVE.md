# rs-archive-clone diagnostic: dispatch blocked

## Scope and status

Run `diagnostic-rs-archive-20261021-v1` stopped on an infrastructure failure
before either agent started. This is neither a completed control pair nor
smoke/scored qualification. Do not reuse its occupied DSH identity or dispatch
Bonsai alone.

The approved manifest was
`sha256:9d45369bb55d8cb40346ee9e02d68bc576605fd73c76bc846b5ae5debb952683`
(`/opt/tb4/frozen-diagnostic-v2.json` on the guest). It binds DeepSeek V4 Flash
at `https://api.deepseek.com/anthropic`, reasoning off, no fallback, serial
DSH then Bonsai, and one attempt each. Both have a 3600-second wall timeout;
Bonsai has 200 turns, while DSH's turn limit is unknown/unset. This is not
an equal-turn experiment.

## Retained observations

| Observation | DSH | Bonsai |
| --- | --- | --- |
| Dispatch | Reserved, failed before agent setup | Not dispatched |
| Terminal reason | Infrastructure: duplicate exclusive trial lock | Not run |
| Task model calls / usage | No agent started; no task usage receipt | Not run |
| Verifier result | Not run | Not run |
| Checkpoint timeline | Not applicable | Unavailable: not run |
| First runnable requested deliverable | Unavailable | Unavailable |
| First direct verification of deliverable | Unavailable | Unavailable |
| Investigation / implementation milestones | Unavailable | Unavailable |
| Provider billing | Unknown (`null`) | Unknown (`null`) |

The separate approved diagnostic-request phase qualified two requests and
retained evidence digest
`sha256:10004af75ee2001f932420c2221027a92e5fa67b7296c1f6e9822252f6bf28c2`.
Raw usage: input/output/cache-read tokens `140/23/128` and `177/2/128`,
cache creation zero for both. Provider billing is null. This is request
qualification, not task performance or smoke credit.

Guest retention root:
`/var/lib/tb4-docker/diagnostic-rs-archive-20261021-v1`.
The DSH slot directory is
`diagnostic/7603e7abb78cecf9e71b697ac2606f73335395d4c8f82f33df3c7587f6362dcb`;
it contains `reservation.json` and no result receipt. Its raw image remains at
`/var/lib/tb4-docker/workspaces/tb4-durable-7603e7abb78cecf9e71b697ac2606f73335395d4c8f82f33df3c7587f6362dcb.ext4`,
536870912 bytes, SHA-256
`8abfa6bc3a2c9038f058fe208ac61ac53e159f6879c2ee30ed97832fc7fa9561`.
After failure, guest inspection found no running Docker containers and no
mounted trial workspace. No image was mounted or deleted during inspection.

## Demonstrated defects and bounded repairs

1. Keyless real-binary staging exposed that trusted harness notes use system
   role, while the receipt scanner only inspected user role. Capture now
   supports the actual producer, and its regression fixture uses the real
   provenance constructor. The guest test proves the injected checkpoint is
   present both in compact receipts and the Anthropic provider request's
   system field. This changes capture, not recovery guidance or permissions.
2. Paid dispatch exposed duplicate lock ownership: diagnostic `main` acquired
   `tb4-trial.lock`, then `LimaExecutor.run` attempted to acquire it again on a
   different descriptor and raised `BlockingIOError`. The redundant entry
   lock is removed; the executor retains exclusive lock ownership through
   verified teardown. An entry-point regression checks that the executor can
   acquire its lock.

## Verification and remaining gate

Rust check, checkpoint tests, formatting, strict all-target/all-feature Clippy,
locked full tests (4475 unit tests plus 19 integration tests), and warning-free
release build passed for the capture repair. Real-binary guest staging passed.
After review, receipt validation rejects zero checkpoint ordinals and malformed
stage/fingerprint types with bounded `ValueError` failures. Local Harbor
discovery passes 94 tests (11 skips); affected lifecycle/diagnostic suites pass
15 tests both locally and on the guest, and all seven Rust checkpoint tests pass.
After the lock repair, affected guest suites pass 29 tests (one skip),
including the entry-point lock regression.
Broad guest discovery is not green: unrelated baseline comparison imports use
Python forward annotations incompatible with guest Python 3.12. Request tests
need the pinned amd64 Node binary run inside its container, not directly on
the ARM guest.

Focused diagnostic commits intentionally depend on existing uncommitted Lima
infrastructure/integration wiring, per the operator's selected commit scope.

Next: qualify the repaired launcher keylessly, then request a separately
approved fresh run identity, freeze new implementation bytes, and obtain new
manifest/phase-bound request and pair approvals. Do not reuse this run or its
approvals after changing implementation bytes. No evidence here supports a
Bonsai prompt/policy change, a model capability claim, or a turn-budget increase.
