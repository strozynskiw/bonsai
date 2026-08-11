# Issue 187: bounded finalization qualification

Qualification date: 2026-08-11

Result: **not qualified**. The deterministic regression and repository gate are
green, and the replay finished below the turn/time/cost budgets, but the replay
exceeded the full-gate budget, repeated one fixed-interval wait, and did not
produce a completed independent review result.

## Candidate and replay contract

- Candidate source: `241d54e95b7c7d810900a001cd7185496c6ba001` plus the
  test-only issue-187 working-tree patch
- Candidate binary SHA-256:
  `741fbb105bfa3d451372a67a5307e28de78dc301fd985ebccffcf7dfea70fc2f`
- Historical workload: `perf(agent): bound finalization and verification work`
  (`eff11ae50c8206320a30b09505f301ad1b57da21`)
- Detached baseline: `c13936fbd21ec7a4b9a35883573a05da7029327d`
  (`eff11ae^`)
- Provider/model/effort: `codex` / `openai/gpt-5.6-sol` / `xhigh`
  (automatic self-review used the recorded `medium` override)
- Execution policy: `yolo`; sandbox off; sandbox network restriction off
- Acceptance set: `cargo fmt --all`, focused regressions, `cargo test --locked`,
  and `cargo clippy --all-targets --all-features -- -D warnings`

These values were reconstructed from local session 155 records. Its session row,
usage ledger, provider setting, user preferences, plan, tool history, and
verification records agree on the model, reasoning, policy, baseline workload,
and acceptance commands. The replay used an isolated `BONSAI_HOME`, transcript,
and detached worktree; its diff was not merged.

## Deterministic coverage

`coding_run_finishes_review_repairs_before_starting_one_final_gate` now scripts
seven provider responses: initial write, terminal candidate, repair write,
rejected duplicate `agent: review`, repaired terminal candidate, final-gate
command, and verified terminal response. It asserts the synthetic rejection tool
result reaches the next provider request, one automatic review is fixed, and one
verification run passes.

The focused test passed. The implementation-tree gate also passed:
`cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test --locked` (4,072 passed, five ignored), and
`cargo build --release --locked`.

## Replay results

| Measure | Requirement | Result | Status |
| --- | ---: | ---: | --- |
| Parent turns | ≤90 | 50 | pass |
| Bounded-inspection tools / tool-call turn | ≥1.5 | 16 / 5 = 3.20 | pass |
| Automatic implementation reviews | ≤1 | 1 | pass |
| Security reviews | none unless needed | 0 | pass |
| Full-gate attempts | ≤2 | 7 total; 4 executed; 1 passed | **fail** |
| Fixed-interval wait sequences | 0 | 1 (`bg-1` waited twice) | **fail** |
| Persisted wake registrations / task | ≤1 | 0 | pass |
| Active run time | <45 min | 1,123,795 ms (18.73 min) | pass |
| Cache read tokens / rate | report | 4,387,840 / 88.51% | — |
| Actual / no-cache cost | <$10 actual | $6.022975 / $25.768255 | pass |
| Final tests / clippy | green | green | pass |
| Malformed-state coverage | preserved | full locked suite green | pass |
| Unresolved Major/Blocker findings | 0 | not established | **fail** |

The sole automatic review was parent-interrupted at the outer replay cutoff before
returning a result. Its persisted counters are zero, but that is not evidence of
an independently reviewed clean diff, so the unresolved-finding criterion is not
claimed.

The first replay process attempted two complete gates before interruption.
Operator continuations added five more attempts while correcting replay-policy
mismatches: one permission-blocked attempt, two sandbox-on executions that
failed environment/network-sensitive tests, one additional environment-blocked
attempt, and the final sandbox-off pass. Every attempt remains in the local
trace; none is excluded from the ticket's full-gate count.

## Miss classification

Classified under #146, `feat(storage): separate active compute from queue and
human wait time`. The outer process cutoff interrupted the one review, while
subsequent permission/environment-policy continuations mixed active work,
blocked execution, and harness correction in the same cumulative session.
Issue 187 remains qualification-only: no telemetry, scheduler, wake, or runtime
remediation was added.

## Local artifacts

Raw traces remain under `target/eval-qualification/issue-187/artifacts/`.

| Artifact | Local path | SHA-256 |
| --- | --- | --- |
| Combined transcript | `target/eval-qualification/issue-187/artifacts/transcript-combined.jsonl` | `8ff09c98c06bd883482d5296fb2114ff0bde8ed8462dd9843cfe3dc7dd3f9697` |
| Isolated database | `target/eval-qualification/issue-187/artifacts/isolated-bonsai.db` | `703068375fe3055df10753282561605a5e1745d9b543c82861733713b8c63f29` |
| Qualification report | `target/eval-qualification/issue-187/artifacts/qualification-report.json` | `9e730b93e97329c940b3a77f2700eacc30bc491599a140e9a04a55941e9e64c1` |
| Summary | `target/eval-qualification/issue-187/artifacts/summary.md` | `673d21314c865f101b738a251c23378398d56f40771afb91de80a9f84c06812a` |
