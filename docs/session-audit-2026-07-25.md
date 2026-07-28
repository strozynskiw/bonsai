# Session audit: 2026-07-25 through 2026-07-28

## Scope and data quality

- Cutoff: 2026-07-25 00:00 Europe/Berlin (2026-07-24 22:00 UTC).
- Snapshot end: 2026-07-28 14:32:23 UTC. `BONSAI_HOME` was unset, so the
  source-defined default store was the active database. It was opened read-only
  with `query_only=ON`, checked as healthy, and copied through SQLite's backup
  API to a temporary point-in-time snapshot for consistent analysis.
- Population: 57 sessions, 2,120 messages, 2,246 tool calls, 2,654 usage turns,
  and 1,305 authorization decisions. Session status was 30 completed, 22
  interrupted, and 5 active.
- Only 21 sessions contained tool calls or usage turns. Nineteen of the 22
  interrupted sessions had no tool calls, and interrupted sessions averaged
  9.1 seconds of active-run time, so interruption is not treated as task
  failure by itself.
- Message rows have no timestamps; tool absolute timestamps are anchored to
  persistence flushes; tool results retain rendered text rather than structured
  exit/error fields; planned execution batches, verification-skip reasons, and
  user satisfaction are not persisted. These gaps limit causal claims about
  abandonment, unnecessary serialization, and completion quality.
- Representative review used redacted tool/status sequences from six completed
  sessions across the observed size range and all three interrupted sessions
  that had tool activity, with the full completed cohort as the comparison
  group. No prompt, file, command, result, project, or path text was copied.

## Strongest recurring findings

1. **Full-plan writes impose avoidable empty-array failures (high confidence,
   medium impact, low repair risk).** `plan_replace_draft` failed 6 of 17 calls.
   Five were schema failures across five sessions; four separate sessions
   supplied a valid flat task list but omitted only the empty `phases` array.
   Every schema failure was followed immediately by a successful replacement
   (mean sequence gap 1.2), showing recoverable round-trip waste rather than a
   bad plan. In `src/tool/plan.rs`, `ReplaceDraftArgs` requires every `Vec`
   during deserialization and the JSON schema marks all four collection fields
   required. `src/agent/run_loop_executor.rs` therefore rejects an omitted
   empty collection before domain validation can apply its natural empty
   default.

2. **One common grep compatibility alias still bounces (high confidence, low
   frequency, very low repair risk).** One failed call used the cross-harness
   `head_limit` field together with the already-supported `output_mode` alias.
   `src/tool/grep.rs` maps `output_mode` and `include`, but not `head_limit`, to
   Bonsai's canonical `limit`. The observed value was a numeric string, so the
   repair must both rename and parse it while preserving canonical-field
   precedence and existing limit validation.

3. **Serial model tool emission dominates long runs, but prompt-only escalation
   has not shown an immediate effect (high impact, high confidence in the
   signal, low confidence in a safe local fix).** Of 1,327 persisted execution
   groups, 919 (69.3%) contained one call. There were 24 cheap-inspection
   streaks of at least three turns and 37 such turns after the first three-call
   advisory point. The current batching hint appeared 21 times in 12 sessions;
   the next assistant action was another single tool in 20 cases and a final
   answer in one, never a multi-tool turn. Parent prompts grew by 82,112 tokens
   on average across the 21 usage-bearing sessions. Stronger rejection would
   also block genuinely dependent reads, so this remains an eval/policy
   follow-up rather than an enforcement change.

4. **Most other failures are expected feedback or already-contained behavior.**
   Overall tool failure rate was 173/2,246 (7.7%). Bash accounted for 103:
   92 ordinary non-zero commands, 6 permission/sandbox failures, 3 timeouts,
   and 2 historical schema failures predating the current descriptive-field
   repair. `apply_patch` had 8 context mismatches and 2 duplicate-file-section
   rejections across six sessions; `edit` had 14 missing and 4 ambiguous
   anchors plus one read-before-edit rejection. Exact consecutive repeats were
   almost entirely expected polling/control behavior (35 of 37 were `tasks` or
   `terminal`); the remaining two were one successful reread and one bounded
   failed-write retry.

5. **Existing containment mechanisms generally worked.** Seventeen read reuses
   avoided 56,111 rendered characters; the read-storm guard rejected three
   requests in one session; five transport retries across two sessions all
   recovered on the second attempt. Two compactions saved 220,696 tokens, and
   episode rewrites recorded 209,295 saved tokens. There were 12 authorization
   denials; only one ask-mode subject was denied twice, which is too little
   evidence to change permission semantics.

6. **Verification evidence is incomplete, not yet proof of false completion.**
   Seventeen completed sessions recorded file changes; 12 had persisted
   verification and 5 did not. The database does not record whether checks were
   disabled, undetected, or explicitly skipped, so forcing verification would
   risk overriding configured behavior. This is a measurement follow-up.

## Selection

| Candidate | Impact | Confidence | Risk / verification cost | Decision |
|---|---:|---:|---:|---|
| Default omitted plan collections to empty while retaining domain validation | Medium | High | Low | Implement |
| Map numeric `head_limit` to grep `limit` | Low | High | Very low | Implement |
| Reject serial inspection turns after a batching hint | Potentially high | Low | High | Reject; dependent reads are valid |
| Loosen patch matching or merge duplicate file sections | Medium | Medium | Medium/high | Reject; could alter edit intent |
| Weaken partial/stale read-before-write checks | Low | High | High safety cost | Reject |
| Force verification whenever a changed session completes | Potentially high | Low | Medium | Reject until skip reasons are measurable |

The selected changes stay in tool schema/coercion code, preserve plan domain
validation, read-before-write, permissions, cancellation, and untrusted-content
boundaries, and require no new dependency. Sanitized regression fixtures should
reduce the observed replay from five selected schema/alias failures to zero:
four omitted-empty-array plan calls and one grep alias call, each without an
extra recovery turn.
