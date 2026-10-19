# Generic contract checkpoint: local-suite full pass

## Result

The first approved Bonsai-only validation achieved **30/30 strict successes** and
**30/30 correct in-scope artifacts**: all ten frozen tasks passed all three
attempts. Independent offline verification reported no failures. Paid iteration
stopped at this full pass: 30 of the approved maximum 90 attempts were used.
DSH was not rerun for this validation.

| Metric | Recovery-v1 | Contract-v1 |
|---|---:|---:|
| Strict completed successes | 27/30 | **30/30** |
| Correct artifacts | 27/30 | **30/30** |
| Median wall time | 7.624 s | 10.260 s |
| Mean wall time | 8.470 s | 11.702 s |
| Nearest-rank p95 | 13.244 s | 26.102 s |
| Model requests | 139 | 163 |
| Reported input + output tokens | 1,730,729 | 2,182,515 |

Contract-v1 reported 2,135,773 input tokens, 46,742 output tokens and 1,736,192
cache-read tokens. Its local estimated cost was $0.093268; actual billing is
unknown. These are descriptive observations, not a controlled causal estimate.
The full pass required more time and requests than recovery-v1.

## Generic changes, not benchmark answers

- Track unresolved failed verification across intervening reads and edits;
  clear it only after a matching successful command/workdir retry. Ignore
  presentation metadata when identifying that retry.
- Recognize ordinary Node test commands as verification without treating
  arbitrary application execution as testing.
- Give bounded recovery guidance and remaining-turn warnings without extending
  the hard turn limit or bypassing completion gates.
- After the first agent-owned mutation in a parent coding task, deliver one
  behavioral-contract checkpoint on the next normal model request. Ask the
  agent to compare requirements and interfaces against its implementation,
  identify untested outcomes, and derive expectations from the specification.
  Stateful/concurrent work should trace transitions and terminal outcomes.
  Reuse current evidence rather than rerun unchanged checks.

The checkpoint contains no task names, expected outputs, algorithms, reference
solutions or hidden assertions. It neither adds a model turn nor authorizes
new effects. The static coding prompt remains unchanged. Offline regressions
exercise real writes, checkpoint timing and hard-budget behavior.

The runner now supports explicit manifest-frozen harness selection, permitting
Bonsai-only validation within the approved spending bound. This changes the
schedule only; no task, visible check, independent check or scoring criterion
was weakened.

## Retained provenance and limits

Private local evidence: `target/comparison/suite-contract-v1/`, including
`manifest.json`, all 30 reserved attempt directories, process results, final
workspaces, and `verified-suite.json`. Failures and interrupted identities would
remain occupied; there were none in this run.

- Frozen task digest:
  `c137863c5186e8c8ec5cc7d5fe33342d3c1b67768fcc3d7fdd15d699c50b6e75`.
- Evaluated binary SHA-256:
  `f78ec7e524efe2022ac69867601f0975e1d2616df39d1133af1996504c85f989`.
- Model: `deepseek-flash`, thinking off; Node `v24.6.0`.
- Three attempts per task, original 120-second per-attempt timeout and hard
  12-turn model budget. Success requires complete execution, exit zero, correct
  artifacts and permitted change scope—not merely passing visible checks.

**Source/build distinction:** the paid binary used a system-role checkpoint.
During the paid run, offline compatibility checks found that this prevented
whole-episode archival because system rows are protected. Final source changes
only that checkpoint to a named non-human Harness user-role advisory, preserving
its content and one-shot timing while leaving pinned/system protections intact.
The 30/30 result therefore applies to the frozen binary above, not to that later
role repair. The role repair is validated separately by checkpoint, episode,
continuity, replay and release-gating regressions; no further paid run is implied.

## Final-source verification

After the named-row repair, the final combined gate passed: `cargo fmt --all`,
`cargo check --locked`, focused checkpoint/self-review and both mock-suite
regressions, refreshed keyless exact-stream replay, all-target/all-feature
clippy with warnings denied, `cargo test --locked`, warning-free locked release
build, and `git diff --check`. Full tests: 4,467 unit tests, nine performance
harness tests and ten surface-smoke tests passed; six manual tests were ignored.
The local runner's nine tests and immutable 60-identity baseline consistency
check also passed. The runner regression confirms Bonsai-only execution does
not require the unused DSH checkout; selecting DSH retains checkout validation.
Focused security review found no authority/intent bypass in
the named non-human advisory.

This is a small, reused custom JavaScript suite, **not Terminal-Bench 4**, a
held-out generalization test, proof of causation, or a guarantee of universal
coding reliability. It establishes the requested full-pass run on the local
suite without task-specific workarounds.
