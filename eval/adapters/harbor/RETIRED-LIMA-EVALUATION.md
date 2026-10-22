# Retired Lima evaluation experiment

The machine-specific paid Lima orchestration and CPU-subset expansion were
retired on 2026-10-22. The generic Harbor adapters, comparison harness, private
checkpoint receipts, and opt-in headless unlimited budgets remain available.
This is not a claim that the retained Harbor harness is ready for another paid run.

## Recovery

Before cleanup, the full adapter directory and modified product sources were
saved locally in `.git/bonsai-archives/paid-eval-20261022/`:

- `source.tar.gz`: tracked and untracked experiment source, excluding bytecode.
- `uncommitted.patch`, `base-commit.txt`, `status.txt`, `contents.txt`.
- Archive SHA-256: `c5fa8ec46849770a5bedf3bf3fc44c3e92743b14e44eec107494a4b6ce469ce3`.

This archive is **local only**, not included in clones or pushes. Preserve it
separately before deleting this checkout. Extract into a separate scratch
checkout for investigation, not over current sources. Committed predecessor
code is also available in Git history through `fbc0ad82`.

Source retirement does not stop/remove the VM, revoke credentials, alter its
firewall, withdraw stored approvals, or delete retained result images. Those
are separate operational actions. Do not relaunch occupied attempt identities.

## Last diagnostic pair

Run: `diagnostic-rs-archive-20261022-v5`, task `rs-archive-clone`.
Manifest: `sha256:d7f53732bf6b81474c5273afa61f52af26eda9b70f3855a05e545684389d5f01`.
DeepSeek V4 Flash, reasoning off; one DSH attempt followed by one Bonsai attempt.

- DSH: reward 0; 2 tests passed, 55 failed. Retained attempt duration 3793.030918s.
  Normalized usage and billing unavailable.
- Bonsai: infrastructure-invalid, reward null, verifier never ran. Adapter's
  post-run binary digest command failed with `Qualification requires preinstalled
  b3sum`; this generic exception does not prove why the command failed.
  Final agent text reported the requested executable was not created despite
  structured status `completed`. Retained attempt duration 862.059572s.
  Reported input 44,284,635 tokens, cache read 43,064,960, output 53,305;
  billing unavailable. This is not a valid paired capability score.

Guest evidence root: `/var/lib/tb4-docker/diagnostic-rs-archive-20261022-v5/`.
Retained images under `/var/lib/tb4-docker/workspaces/`:

| Agent | Image basename | SHA-256 |
|---|---|---|
| DSH | `tb4-durable-b07238be85e9bb00de98df11c98e964865ece0de16ce81fd899d21bdcf3170bf.ext4` | `ff52ced94e1913f652405708c8c8cb3d6465c5a567f969691ff6d56200c19f7a` |
| Bonsai | `tb4-durable-6e870840f878daa06d79ec8a39df6dc42ecb2a6ea4b6cad845d107b3b4a05fda.ext4` | `76a9ddb19cd214bb906a4adb803169670a2b50bb127df1b15cd4ca133ee23510` |

At run close, both hashes matched receipts, neither image was loop-mounted,
and no containers were running. This records that observation, not live status.
See `DIAGNOSTIC-RS-ARCHIVE-V2.md` for earlier, separately contaminated attempts.

## Before evaluating again

Start with a small deterministic task and exercise the entire adapter/verifier
path keylessly before spending. In particular, check digest prerequisites before
agent execution, preserve terminal JSON independently of stderr, distinguish
agent status from artifact correctness, and ensure orchestration outlives its
local monitoring connection. A different benchmark need not reuse Lima at all.
