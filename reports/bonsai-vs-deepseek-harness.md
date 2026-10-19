# Bonsai vs. DeepSeek Harness

## Executive summary

Bonsai has concrete strengths in filesystem mutation integrity, unified effect authorization, optional network denial, and terminal deployment simplicity. DeepSeek Harness (DSH) is further ahead in multi-surface integration, plugin lifecycle infrastructure, bounded tool scheduling, and production-path recorded-session testing.

The recommended direction is **provider-independent coding with verifiable execution and recovery**. Adopt useful mechanisms and testing practices, not DSH's entire plugin-first architecture.

This report proposes improvements; it does not authorize implementation or change the committed roadmap.

## Scope and evidence limits

- DSH snapshot: [`687f282e86`](https://github.com/deepseek-ai/deepseek-harness/tree/687f282e86), version `0.2.1-rc.2`, cloned from the local `deepseek-harness` repository. Uncommitted changes in that source checkout were not included.
- Bonsai: the current working checkout during the comparison, including existing work in progress. Findings are not a comparison of two immutable release binaries.
- Method: source and documentation inspection, scoped runtime/product/security investigations, and inspection of Bonsai's live GitHub issues and milestones.
- No builds, installations, coding-task benchmarks, or runtime security tests were performed. There is no measured basis here for claiming better task success, lower cost, lower latency, or lower memory use.
- DSH describes itself as developer-preview software. Experimental packages and automation-only interfaces are distinguished below from broader product support.
- Documentation-only architectural claims were not treated as proof of runtime behavior. Security observations concern the inspected boundaries, not a comprehensive audit.

## Where Bonsai is stronger

### 1. Filesystem mutation integrity

Bonsai uses descriptor-relative filesystem operations, `O_NOFOLLOW`, content preconditions, exclusive temporary files, and replacement through an opened parent directory. The inspected DSH filesystem sandbox resolves canonical paths and checks containment, but explicitly acknowledges a remaining ancestor-swap race.

This is a meaningful implementation advantage for Bonsai's inspected mutation path, not a claim that DSH has a demonstrated exploitable vulnerability.

Evidence: [Bonsai secure filesystem operations](../src/tool/secure_fs.rs#L24); [DSH filesystem sandbox](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/fs/fs-sandbox/src/index.ts#L122).

### 2. Unified authorization and effect accounting

Bonsai declares concrete effects, derives risk from the effect plan, and records authorization decisions with their rule source, autonomy, and sandbox posture. This is a stronger inspected cross-capability accounting mechanism than the DSH shell approval path examined in this comparison.

Keep authorization distinct from successful execution: an approved operation is not evidence that its intended effect completed. Authorization-ledger persistence failures also need to remain visible.

Evidence: [Bonsai action policy](../src/tool/action_policy.rs#L316); [DSH Bash execution and policy resolution](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/shell/tool-bash/src/index.ts#L207).

### 3. Separate network-denial controls

Bonsai supports network denial independently of filesystem write confinement. The inspected DSH Bubblewrap, Landlock, and Seatbelt profiles primarily constrain writes while retaining broad reads and network access.

This favors Bonsai's available confinement posture, but does not imply every Bonsai session enables it. Neither write confinement nor network denial alone constitutes complete secret isolation.

Evidence: [Bonsai sandbox initialization](../src/sandbox/mod.rs#L168); [DSH sandbox profiles](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/sandbox/sandbox-local/src/profiles.ts#L16).

### 4. Terminal deployment simplicity

Bonsai's single Rust binary is a simpler terminal deployment model than a Node runtime with package/plugin composition. Preserve that operational advantage while introducing optional integrations.

This is a deployment comparison, not a performance measurement.

Evidence: [Bonsai overview](../docs/overview.md#L3); [DSH setup](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/README.md#run).

## Where DSH is ahead, and what to adapt

### 1. Multi-surface integration

DSH has Web/desktop infrastructure, SDK packages, and implemented ACP session operations, including initialization, creation, resumption, and prompting. Bonsai currently exposes TUI and headless workflows rather than an equivalent server/SDK product surface.

DSH's ACP server is explicitly automation-only: it intentionally omits DSH-specific presentation and interactive UI features. Do not interpret ACP support as complete human-facing UI parity.

**Adaptation:** extract surface-neutral contracts and expose the existing Bonsai engine through adapters. Preserve common authorization, persistence, cancellation, budgets, and recovery rather than creating a privileged server path.

Evidence: [DSH ACP implementation](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/acp/acp/src/index.ts#L180); [ACP scope and limitations](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/acp/acp/README.md); [Bonsai server issue #46](https://github.com/strozynskiw/bonsai/issues/46).

### 2. Production-path recorded-session testing

DSH's recorded-session tests exercise shipped profiles and compare output, persisted session records, and complete final workspaces. Separate ACP and SDK snapshot suites cover additional interfaces.

Bonsai already has file-state/test graders, replay-oriented tests, and provider-stream recording/replay work in progress during this comparison. The opportunity is to extend those foundations into production-path contracts, not create another recorder.

**Adaptation:** build a keyless scenario corpus covering retry, cancellation, approval, context compaction, interruption recovery, and session resumption. Assert actual workspace effects as well as transcript and ledger output. Use the same behavioral scenarios across supported surfaces where practical.

Evidence: [DSH final output/session/workspace assertions](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/snapshots/session/headless.snapshot.ts#L1410); [Bonsai graders](../src/eval/grader.rs#L64); [Bonsai replay tests](../src/agent/tests/read_efficiency_replay.rs).

### 3. Bounded rolling tool execution

DSH admits parallel calls through a bounded rolling pool, commits results in model order, and rechecks execution classification before starting later calls. Bonsai plans path-conflict-aware batches and executes each batch using `join_all`.

Bonsai's conflict analysis is valuable and should not be replaced with a simple parallel/exclusive flag. A rolling scheduler is a candidate improvement, not an established performance win.

**Adaptation:** benchmark bounded admission and reduced batch barriers while retaining path conflicts, authorization boundaries, deterministic result ordering, and cancellation semantics. Introduce the change only if measured workloads justify its complexity.

Evidence: [DSH rolling pool admission](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/core/agent-loop/src/tool-calls.ts#L199); [Bonsai batch planning](../src/agent/run_loop.rs#L887); [Bonsai batch execution](../src/agent/run_loop/tools.rs#L41).

### 4. Progress-checked context-overflow recovery

DSH recognizes canonical context-window errors and attempts bounded recovery. It retries only after the session request surface advances, including when a pruning phase succeeds but a later summary phase fails. Cancellation takes precedence.

Bonsai already implements preflight compaction and transient provider retries. The inspected code did not reveal an equivalent canonical context-overflow recovery path; this should be confirmed with a focused implementation audit before adding one.

**Adaptation:** add typed overflow classification and progress-checked recovery if absent. Do not blindly resend the same oversized request or replace Bonsai's existing compaction system.

Evidence: [DSH overflow recovery](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/compaction/compaction-basic/src/index.ts#L190); [Bonsai retry policy](../src/agent/retry.rs#L527); [Bonsai preflight compaction](../src/agent/compaction/mod.rs).

### 5. Extension lifecycle and sandbox backend breadth

DSH has implemented plugin management operations with compatibility checks and restoration paths. Its sandbox infrastructure also includes Landlock and partial Windows confinement, beyond Bonsai's inspected Seatbelt/Bubblewrap support.

**Adaptation:** borrow explicit compatibility, lifecycle, and rollback contracts for future extension distribution. Add Landlock through Bonsai's existing roadmap. Do not equate a marketplace or more backends with stronger security across every boundary.

Evidence: [DSH plugin operations](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/boot/plugin-manager/src/operations.ts#L285); [DSH backend selection](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/sandbox/sandbox-local/src/index.ts#L160); [Bonsai Landlock issue #17](https://github.com/strozynskiw/bonsai/issues/17).

## Capabilities that should not become duplicate projects

- **Provider independence:** Bonsai has declarative connections and provider switching, but DSH is not DeepSeek-only. Its pi-ai adapter supports configurable provider routes and model catalogs. Compare provider correctness and usability rather than counting logos. See [DSH pi-ai adapter](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/llm/llm-pi-ai/src/index.ts).
- **Compaction:** Bonsai already has preflight compaction, summaries, output pruning, and task episodes/recall. The proposed gap is specific overflow recovery, not basic context management.
- **Worktrees:** Bonsai already uses recovery worktrees for mutating headless runs. DSH's inspected worktree package is experimental and serves a different session-directory workflow. See [Bonsai recovery](../docs/recovery.md) and [DSH experimental worktrees](https://github.com/deepseek-ai/deepseek-harness/blob/687f282e86/packages/experimental/worktree/README.md).
- **Evaluation and replay:** extend existing graders and recording/replay work; do not create parallel infrastructure.
- **Server, ACP, plugins, and Landlock:** existing GitHub issues already cover these areas. Refine their acceptance criteria rather than filing duplicates.

## Prioritized improvement plan

Priority expresses a recommendation, not a newly approved release commitment. The live GitHub milestones remain the delivery authority.

| Priority | Work | Acceptance criteria | Existing roadmap |
| --- | --- | --- | --- |
| P0 | Activate performance regression gates | Review and commit reference baselines for all four supported targets; material regressions fail release qualification. At inspection, `eval/baselines/performance/` contained only its README. | [#18](https://github.com/strozynskiw/bonsai/issues/18), milestone 0.6.0 |
| P0 | Extend replay into production-path scenarios | Keyless tests cover retry, cancellation, approval, compaction, recovery, and resume. Assert workspace effects, output, and relevant persisted records. Reuse stream recording/replay already underway. | Extend existing eval work; select an owner before scheduling |
| P1 | Progress-checked context-overflow recovery | Confirm the gap; classify canonical overflow; compact and retry within a bound only after meaningful reduction; preserve cancellation and usage accounting. | New focused work only if the audit confirms absence |
| P1 | Evaluate bounded rolling scheduling | Compare mixed-duration tools against current batching. Bound concurrency and retain path-conflict safety, authorization, deterministic commits, and cancellation. Ship only with measured benefit. | Coordinate with [#144](https://github.com/strozynskiw/bonsai/issues/144); subagent fan-out and general tool scheduling are distinct |
| P1 | Deliver the planned server/ACP preview | Surface-neutral protocol, authenticated local server, shared engine semantics, streamed events, interrupt/approval/resume behavior, and cross-surface contract tests. Evaluate existing clients before building a bespoke UI. | [#21](https://github.com/strozynskiw/bonsai/issues/21), [#46](https://github.com/strozynskiw/bonsai/issues/46), [#77](https://github.com/strozynskiw/bonsai/issues/77), [#78](https://github.com/strozynskiw/bonsai/issues/78), [#79](https://github.com/strozynskiw/bonsai/issues/79), milestone 0.5.0 |
| P2 | Broaden confinement and extension distribution safely | Landlock backend with explicit posture; later, pinned extension manifests, explicit grants, compatibility checks, and rollback before broad marketplace distribution. | [#17](https://github.com/strozynskiw/bonsai/issues/17), milestone 1.1; [#53](https://github.com/strozynskiw/bonsai/issues/53), [#54](https://github.com/strozynskiw/bonsai/issues/54), [#55](https://github.com/strozynskiw/bonsai/issues/55), milestone 1.4 |

### Suggested sequence

1. Establish trustworthy baselines and production-path replay evidence using existing infrastructure.
2. Close focused runtime gaps and evaluate scheduler changes with those regression protections.
3. Complete the already-planned server/ACP preview without splitting engine semantics.
4. Expand platform and extension reach only after lifecycle, compatibility, and safety contracts are tested.

These tracks can overlap where ownership is independent. They should not displace already agreed milestone commitments without review.

## What not to adopt now

- Do not rewrite Bonsai around Cordis or a universal plugin architecture without a demonstrated requirement.
- Do not build a large marketplace before secure installation and compatibility contracts.
- Do not begin a bespoke browser UI before evaluating existing clients against the server preview.
- Do not claim coding-quality superiority from implementation language, feature counts, or coverage percentages.
- Do not weaken filesystem conflict analysis or authorization to copy a faster-looking scheduler.

## Measurement and decision rules

Use repeatable tasks with the same model, task inputs, repository state, budgets, and verification criteria when evaluating harness changes. Record success/failure, verified workspace outcome, billed tokens/cost, latency, resource peaks where relevant, and interruption/recovery behavior. Repeat live-model trials where nondeterminism matters; keep keyless replay as regression evidence rather than a substitute for live task-quality evaluation.

For every adopted idea, require a named owner, a specific observed gap, a focused acceptance test, and a compatibility/safety review appropriate to its boundary. Preserve Bonsai's existing strengths while making their product value measurable.

## Roadmap documentation discrepancy

[Bonsai's overview](../docs/overview.md#L84) groups a server API with post-1.0 features, while live issues place the experimental server and ACP preview in 0.5.0. The distinction is experimental preview versus a stable protocol; align that wording when updating product documentation. No documentation correction or GitHub issue mutation was performed as part of this comparison.
