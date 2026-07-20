# bonsai Roadmap

> **Mission.** Build the most capable and trustworthy provider-independent coding
> agent: a fast Rust binary that can inspect, change, run, debug, verify, and review
> real software with high autonomy and measurable quality.

Last revised: 2026-07-19.

This file contains **unfinished work only**. Completed implementation belongs in
`README.md`, tests, release notes, and git history. The current codebase has completed
the previously planned coding-breadth milestone; the next product milestone is the
production release candidate, followed by 1.0.

Priority labels:

- **P0** — blocks the release candidate or 1.0.
- **P1** — required for the intended 1.0 product quality.
- **P2** — desirable for 1.0, but may move to 1.x if the safe fallback is already shipped.
- **Later** — explicitly outside the 1.0 critical path.

---

## The 1.0 product contract

bonsai 1.0 is a production-grade, local-first terminal coding agent for macOS and
Linux. It is not merely a chat TUI and it is not yet a hosted development platform.

| Area | 1.0 promise |
|---|---|
| Coding loop | Inspect → edit → run → diagnose → fix → verify → review, without requiring the user to manually restart the loop after a normal failure. |
| Trust | Secrets never enter SQLite; users choose session-only, OS keyring, or OS-protected file storage. Effectful actions are authorized before their first side effect, and supported sandboxes fail safely and explain their posture. |
| Correctness | Release-gating agent evals, public benchmark results, and a completion contract report what was and was not verified. |
| Breadth | First-class Rust, TypeScript/JavaScript, Python, and Go workflows, with graceful grep/bash fallback for other languages. |
| Providers | Stable multi-provider operation, an official OpenAI API connection, local OpenAI-compatible models, and provider-independent model metadata. |
| Extensibility | Trusted skills, custom agents, hooks, and MCP over stdio/HTTP, including authenticated remote MCP. |
| Surfaces | TUI and headless/CI mode have compatible permissions, budgets, persistence, cancellation, recovery, and completion semantics. |
| Operations | Reproducible signed releases, documented upgrades, diagnostics, support bundles, and tested recovery from interruption and provider failure. |

A server API, browser/desktop UI, Windows, an SDK, a plugin marketplace, hosted agents,
and general product-roadmap management do not block 1.0. They remain ordered 1.x work.

---

## Current baseline

The detailed shipped surface lives in `README.md`. The release-candidate work starts
from this baseline:

- The complete coding loop now advances through an explicit per-run turn coordinator;
  parallel-safe tools, self-review, verification, completion reports, persistent sessions,
  budgets, recovery worktrees, and distinct user-facing agents and delegated subagents are
  implemented. Built-in subagent identities are reserved, with model/enable settings stored in
  SQLite rather than materialized as switchable custom agents.
- Rust, TypeScript/JavaScript, Python, and Go have tree-sitter structure, repo maps, symbol
  tools, and managed language-server support. Web search/fetch and native interactive
  PTY workflows are implemented.
- Providers are catalog-driven across the supported wire transports, with first-run
  authorization/model selection, protected credential choices, reasoning controls,
  prompt-cache accounting, model fallback, and local-model support.
- Workspace trust, sandbox enforcement, effect plans, the authorization ledger,
  read-before-write freshness, redaction, protected-path handling, and canonical
  untrusted-content framing are implemented.
- The release failure matrix covers expired authorization, offline startup, mid-stream
  network loss, removed provider/model targets, and malformed layered configuration with
  bounded retries, truthful outcomes, preserved state, recovery guidance, and resumable
  headless sessions where recovery is supported.
- The 1.x compatibility contract versions config and machine-readable headless output,
  freezes the public-alpha fixtures, preserves renamed provider state, creates a private
  database snapshot before upgrades, and automatically rolls back failed migrations.
- Skills, custom agents, hooks, MCP stdio/HTTP/OAuth, provider management, themes,
  settings, session resume, planning, memory, and default-on task episodes are implemented.
- Onboarding, `bonsai doctor`/`/doctor`, signed-manifest generation, four-target release
  builds, checksums, SBOM generation, dependency/security checks, installer foundations,
  local benchmark adapters, and cross-platform Rust CI are implemented. Public release
  documentation is audited, and CI locks its versions, commands, headless flags, settings,
  supported targets, and registry-backed product counts to the binary and release tooling.
- TUI/headless parity is qualified by shared permission, budget, persistence, cancellation,
  recovery, provider-failure, and completion-report acceptance fixtures plus real-binary
  headless tasks and native-terminal TUI smoke coverage on macOS and Linux CI.
- Release performance tooling records raw and p50/p95 native-binary samples for startup,
  idle CPU, first output, shared-snapshot persistence, RSS, binary size, context growth,
  cache reuse, and representative task cost. Eval efficiency tolerances gate material
  regressions, and each target gate activates when its reviewed baseline is committed.
  RC soak evidence binds the tag, manifest, binary hashes, passing performance reports,
  elapsed time, observations, and typed incidents.
- Accessibility essentials are shipped: chip-safe composer undo/redo, reduced-motion and
  linear screen-reader modes, preserved no-color/high-contrast theming, and small-terminal
  permission/question modal coverage.
- Support diagnostics are shipped: the opt-in redacted JSONL lifecycle log (runs,
  turns/providers/tools, effects, budgets, terminal outcomes) and the user-reviewed
  `/bug` / `bonsai bug` support bundle, with prompts/code/credentials/environment values
  excluded and log tails opt-in per bundle. OpenTelemetry export and a timeline inspector
  remain explicitly out of 1.0.

No completed checklist is repeated below.

---

## Remaining path to 1.0

Work through the phases in order. Remote benchmark runs, public-repository administration,
and soak time can proceed in parallel once their prerequisites are ready.

### Phase 1 — Prepare public RC distribution

These items require repository-owner or release-operator access and cannot be completed by
ordinary local coding work alone.

- [ ] **P0 · Public repository and security controls**
  - Decide whether the planned public-history reset is still wanted, perform it if so, then
    flip the repository public.
  - Enable GitHub private vulnerability reporting and verify the public security policy,
    dependency review, advisory workflow, and supported-platform statement.

- [ ] **P0 · Production signing drill**
  - Provision the production Ed25519 private key and matching public key in GitHub without
    exposing either through logs or artifacts.
  - Publish the first signed canonical manifest, install its binary on a clean machine, and
    verify identity/update diagnostics through `bonsai doctor --online`.

- [ ] **P0 · Publish and verify distribution channels**
  - Publish macOS x86_64/arm64 and Linux x86_64/arm64 archives, checksums, completions,
    SPDX SBOM, provenance, and attestations through the real release workflow.
  - Install and smoke-test every supported artifact, verify the attestations independently,
    and ensure unsupported targets fail with the documented support statement.
  - Publish a Homebrew tap/formula only after the signing and clean-install drill passes.
    Self-update remains post-1.0.

**Phase exit:** a public prerelease can be installed through a documented channel and its
binary, manifest, checksum, SBOM, and provenance can be independently verified.

### Phase 2 — Produce release evidence

- [ ] **P0 · Multi-language acceptance**
  - Run representative Rust, TypeScript/JavaScript, Python, and Go repositories through the
    same inspect/edit/verify/review suite, including language-server unavailable fallback.
  - Freeze the minimum release thresholds and retain the prompts, configuration, model,
    effort, policy, budgets, artifacts, and failure classifications needed to reproduce
    every result.

- [ ] **P0 · Public benchmark qualification**
  - On a disk-capable remote runner, run small pinned official SWE-bench Verified and
    Terminal-Bench canaries using the implemented adapters. Local CI and this development
    machine must not download datasets, runtimes, or container images.
  - Review and import redacted artifacts, publish the normalized scorecard, freeze the RC
    regression threshold, and enforce it for release qualification.

- [ ] **P0 · Adversarial release qualification**
  - Run credential, untrusted-content, effect-policy, symlink-race, sandbox, malformed
    stream, cancellation, resume, and migration suites against the release build.
  - Resolve every P0/P1 security, data-loss, authorization, migration, sandbox, and false-
    success defect before starting the final soak.

- [ ] **P2 · Decide the default episode posture from evidence**
  - Observe at least 20 real topic transitions and measure false closes, task success,
    prompt growth, cache reuse, resume integrity, memory, and database growth.
  - Keep episodes default-on only if the data holds. Otherwise exercise
    `BONSAI_EPISODES=0` and move recall polish to 1.x; the safe fallback is already
    shipped.

- [ ] **P1 · Qualify performance and soak the final RC**
  - Review native reports for all four supported targets, commit the matching references,
    and require the exact RC binaries to pass every deterministic and material-regression
    gate; report-only target evidence is not sufficient for qualification.
  - After adversarial qualification passes, pin one immutable tag, peeled commit, signed
    manifest, binary hashes, and passing performance-report hashes. Dogfood those unchanged
    artifacts for at least 336 real hours with passing TUI/headless observations on 14 UTC
    dates and no release-blocking data-loss, security, migration, or task-completion incident.

**Phase exit:** quality, security, compatibility, cross-platform installation, performance,
and soak evidence all refer to the same immutable release candidate.

---

## 1.0 release gate

1.0 ships only when:

- [ ] Every P0 and P1 item in Phases 1–2 is complete or explicitly removed from the 1.0
  contract with a documented safe fallback.
- [ ] No open P0/P1 security, data-loss, authorization, migration, sandbox, recovery, or
  false-success defect remains.
- [ ] Language acceptance and public benchmark results meet frozen RC thresholds with no
  unexplained regression.
- [ ] All four supported artifacts install, run, and verify from public distribution.
- [ ] Upgrade/recovery fixtures and user documentation match the shipped binary.
- [ ] The immutable RC completes the minimum two-week soak without a release blocker.

---

## Remaining plan inputs

Only plans with unfinished work influence ordering:

| Plan | Remaining contribution |
|---|---|
| [`plans/external_benchmark_adapters.plan`](plans/external_benchmark_adapters.plan) | Only remote Phase E remains; it feeds Phase 2 public benchmark qualification. |

The production-readiness backlog is closed (2026-07-16): the saved-plan double-duty fix
(`saved_plans` snapshot table, migration 0018) was its last user-visible defect. Background-output
delta events were rejected as working-as-intended (capped tail + `Lagged` self-heal beats
delta-resync complexity), and the keymap test re-homing stays opportunistic. The audit's remaining
structural refactors are consolidated below and move onto the critical path only if a Phase 2
acceptance bar exposes a concrete dependency.

Completed PTY, extension-runtime, safety, and audit repair checklists are historical evidence,
not additional roadmaps.

## Deferred architecture work

This debt is real but does not delay 1.0 merely because it exists:

- [ ] Extract surface-neutral persisted transcript/tool protocol types and a typed tool-result
  envelope before adding server/SDK surfaces.
- [ ] Split persistent `Agent` state into cohesive domains, finish typed loop-policy
  decisions, and split TUI state/actions/effects by feature before materially expanding
  either subsystem.
- [ ] Decompose Bash execution into policy, shell-session, process, and output services;
  finish typed runtime contexts where long positional dependency lists remain.
- [ ] Split oversized test modules, encode the lint baseline in `Cargo.toml`, and rewrite
  implementation-history comments as durable invariants while touching the owning modules.
- [ ] Replace full-tail background broadcasts with delta/resync events only if profiling
  proves the current render-coalesced clone path material.

## After 1.0

The 1.x sequence remains dependency-ordered. Dates are assigned after the 1.0 RC exposes
real throughput and maintenance costs; release numbers describe outcomes, not deadlines.
Each release must be useful on its own and preserve the local TUI/headless product.

| Release | Primary outcome | Depends on |
|---|---|---|
| 1.1 | Deliver and maintain changes autonomously | 1.0 verification, budgets, effect ledger, and worktrees |
| 1.2 | Work effectively in very large repositories | 1.0 language services plus 1.1 durable task state |
| 1.3 | Use the same agent locally, remotely, and in an IDE | Stable session/event semantics from 1.0–1.2 |
| 1.4 | Safely distribute extensions and team policy | Stable extension and server protocols |
| 2.0 | Coordinate durable work across repositories and runners | All 1.x isolation, policy, knowledge, and platform layers |

### 1.1 — Autonomous delivery and feedback loops

**Goal:** move from one supervised coding session to a bounded worker that can deliver a
change, react to feedback, and show exactly what it is doing.

- [ ] **Durable run controller and execution dashboard**
  - Persist the goal, task DAG, current step, dependencies, attempts, evidence, approvals,
    worktree, budgets, and terminal reason independently of chat history.
  - Pause, resume after process restart, interrupt-and-steer, skip, retry, or cancel one
    step without corrupting completed work.
  - Show current and queued work, agents, branches, risks, checks, token/cost burn, blocked
    authority, and recovery state in one TUI surface and headless event stream.
  - Make retries and event delivery idempotent so a crash cannot apply or post the same
    change twice.

- [ ] **Parallel implementation with isolated worktrees**
  - `/batch <goal>` researches the task, proposes a dependency graph of independently
    reviewable units, and waits for approval before fan-out.
  - Mutating agents run in separate worktrees with scoped paths/tools/budgets. A merge
    queue detects overlapping intent, rebases in dependency order, and sends conflicts
    back to the owning agent with bounded retries.
  - Run unit-level checks before merge and final repository checks after integration;
    preserve every child report and diff in the parent completion evidence.
  - Measure whether fan-out improves wall time without reducing pass rate or multiplying
    token cost beyond a configured ceiling.

- [ ] **Issue-to-PR workflow**
  - `bonsai work issue <url|id>` reads the issue and linked context as untrusted data,
    creates an isolated branch/worktree, implements and verifies the change, and opens a
    draft PR with evidence and remaining caveats.
  - `bonsai review --pr <n>` posts verified inline findings with severity, confidence,
    file/line evidence, and deduplication against existing comments.
  - Support GitHub first through `gh` and its API; keep forge concepts typed so GitLab or
    another forge can be added without rewriting the agent loop.
  - Never push, comment, close, merge, or alter an issue without the configured external-
    write policy and an auditable authorization record.

- [ ] **CI and review autofix loop**
  - Watch a selected PR for failed checks and new review comments; cluster failures by
    root cause, reproduce locally when possible, patch, re-run relevant checks, and push
    one reviewable fix at a time.
  - Rate-limit attempts, ignore superseded commits/comments, stop on repeated root causes,
    and require approval before expanding scope beyond the PR.
  - Integrate optional CodeQL, secret scanning, and dependency-advisory results into the
    security reviewer rather than treating green unit tests as sufficient evidence.

- [ ] **Triggers and monitors**
  - Declarative triggers for schedule, CI failure, issue assignment, PR comment, file/log
    watch, pre-commit, and pre-push events.
  - Every trigger specifies workspace/environment, prompt or skill, autonomy, network,
    allowed external writes, budget, concurrency, debounce/idempotency key, and failure
    notification.
  - Provide dry-run validation and a visible queue. Unknown or project-owned triggers stay
    inert until workspace trust and the extension policy authorize them.

- [ ] **Browser and UI verification**
  - Launch or attach to a local application, navigate, inspect DOM/accessibility state,
    click/type, capture screenshots, read console/network errors, and compare before/after
    states through a bounded browser session.
  - Connect browser actions to the same task evidence as tests and diagnostics so a UI
    change cannot be called verified solely because it compiled.
  - Isolate origins and profiles, gate network and downloads, redact captured content, and
    keep page output untrusted. Prefer a Playwright-backed extension first; promote a
    built-in tool only if evals show repeated-turn savings.

- [ ] **Checkpointing and conversation branching**
  - Add prompt-level checkpoints that can restore code, conversation, or both, plus fork a
    new session from any checkpoint while preserving the original.
  - Base code recovery on observed workspace snapshots/worktrees so Bash, hooks, LSP edits,
    and external changes have an honest boundary; never imply untracked changes are safe
    to rewind.
  - Retention, cleanup, storage cost, and conflicts with user edits are explicit.

- [ ] **Verified multi-agent review**
  - Run focused logic, security, test-gap, performance, and compatibility reviewers in
    parallel for high-risk diffs.
  - A final verifier checks findings against code/tests, merges duplicates, and suppresses
    unsupported speculation before anything is posted externally.
  - Tune the reviewer set by language/change risk; do not pay for a full fleet on routine
    edits.

**Exit bar:** from one issue, bonsai can plan isolated units, implement and integrate the
change, verify UI and non-UI behavior, open a draft PR, react safely to one CI failure and
one review comment, and stop within policy/budget with a complete evidence trail.

### 1.2 — Large codebases, impact analysis, and durable knowledge

**Goal:** stay accurate and economical when a repository is too large to rediscover on
every task.

- [ ] **Incremental multi-language project graph**
  - Index packages, files, symbols, imports, calls, implementations, schemas, endpoints,
    tests, build targets, generated code, and ownership/provenance.
  - Merge LSP, tree-sitter, build metadata, and git evidence while retaining source and
    freshness for every edge; uncertain relationships remain uncertain.
  - Update only affected graph partitions after edits or branch switches and expose a
    compact query tool rather than injecting the graph into every prompt.

- [ ] **Monorepo and multi-root workspaces**
  - Multiple trusted roots per session with root-scoped permissions, sandboxes, memories,
    language services, config, and verification profiles.
  - Model package/build-system relationships across Cargo workspaces, npm/pnpm/yarn,
    Python workspaces, Go modules, and common monorepo coordinators.
  - Let plans and worktrees declare affected packages so unrelated roots are not read,
    tested, locked, or sent to providers.

- [ ] **Change-impact and test selection**
  - Rank affected callers, consumers, schemas, packages, docs, and tests from graph and git
    evidence; explain why each item is included.
  - Run a fast impacted-check set during repair and the configured authoritative suite at
    completion. Learn timing and flakiness, but never silently redefine the project's
    required checks.
  - Detect migrations, API/ABI changes, generated artifacts, dependency lock changes, and
    cross-language contracts that require broader review.

- [ ] **Persistent knowledge sources**
  - `/knowledge add <path|repo|url|mcp-source>` registers versioned library docs, dependency
    source, architecture records, schemas, tickets, and internal references.
  - Hybrid lexical/semantic retrieval returns citations, version/freshness, trust source,
    and the smallest useful excerpt; indexed external material remains untrusted.
  - Support offline use after indexing, incremental refresh, removal/export, per-source
    permissions, and a clear answer when a source is stale or unavailable.

- [ ] **Feature and decision knowledge**
  - Git-trackable feature records capture goals, invariants, public contracts, decisions,
    dependencies, owners, relevant files/tests, rollout, and known debt.
  - Propose updates after verified work, but require normal diff/review policy; do not let
    generated documentation silently become executable instruction.
  - Detect contradictions with code, config, newer decisions, and other feature records
    rather than blindly preferring memory.

- [ ] **History-aware ownership and regression context**
  - Use git history, blame, prior sessions, linked issues/PRs, and failure history to show
    why code exists, which constraints recur, and where regressions previously appeared.
  - Keep people/team data optional and local; do not infer expertise or performance scores
    from commit history.

- [ ] **Retrieval and context evals**
  - Add large-repo tasks that grade file discovery, dependency tracing, test selection,
    stale-source handling, answer citations, tokens, index latency, and memory use.
  - Compare graph/semantic retrieval against grep/repo-map baselines. Features that do not
    improve task outcomes stay optional or are removed.

**Exit bar:** on a representative million-line monorepo, bonsai locates the correct change
surface, explains cross-package impact, retrieves version-matched documentation, runs an
efficient check set plus the required final suite, and completes within published index,
memory, latency, and token budgets.

### 1.3 — Local, remote, and IDE surfaces

**Goal:** make bonsai available where developers work without forking the engine or
weakening local security.

- [ ] **Versioned engine and event protocol**
  - Define stable session, content-part, tool/effect, approval, diff, plan, budget, usage,
    attachment, and lifecycle events shared by TUI, headless, server, SDK, and IDE clients.
  - Add capability negotiation, schema versions, backpressure, replay cursors, reconnect,
    idempotent client commands, and compatibility fixtures.
  - Keep the Rust engine internal to the binary until an actual library boundary is needed;
    do not add a broad `lib.rs` merely to satisfy architecture diagrams.

- [ ] **Authenticated local server**
  - `bonsai serve` exposes session create/list/resume/chat/interrupt/approve APIs plus an
    event stream and artifact access.
  - Bind loopback by default with short-lived scoped credentials, origin protection,
    request limits, audit events, and explicit opt-in for any non-loopback listener.
  - Server sessions use the same workspace trust, sandbox, effects, budgets, persistence,
    and recovery code as the TUI; no privileged server-only shortcut.

- [ ] **Web and IDE clients**
  - Ship the browser client first: transcript, streaming, plan/todos, tool details, diffs,
    checkpoints, file mentions, model/provider selection, and permission/question prompts.
  - Add VS Code next with editor selection/current-file context, diagnostics, side-by-side
    diffs, terminal handoff, and session continuation. JetBrains follows only after the
    protocol and VS Code extension are stable.
  - A Tauri desktop shell may wrap the web client; it does not get a separate agent stack.

- [ ] **Outbound-only remote control**
  - Let a user expose a chosen local session to another browser/device without opening an
    inbound port, using short-lived session-scoped credentials and explicit pairing.
  - The agent and tools continue to run on the user's machine; remote clients can be made
    read-only or denied approvals/external writes.
  - Survive sleep/network interruption with replay from the last acknowledged event and a
    prominent connected-client indicator in every surface.

- [ ] **Remote environment adapters**
  - Run the engine or tools inside devcontainers, Docker/Podman, SSH workspaces, and later
    ephemeral hosted runners while preserving path, git, terminal, attachment, sandbox,
    and artifact semantics.
  - Environment definitions are reviewable and immutable per run; secrets are injected as
    short-lived references, never copied into prompts or snapshots.
  - Cache dependencies/artifacts by content and provenance without sharing writable state
    across untrusted projects.

- [ ] **Typed SDK**
  - Rust and TypeScript clients for session control, event streaming, approvals, custom
    tools, eval harnesses, and embedding bonsai in another product.
  - Generate clients from the versioned protocol and publish conformance tests; SDK APIs do
    not expose internal database or provider implementation details.

- [ ] **Windows support**
  - Native process groups/PTY, path and shell semantics, clipboard/images, credential
    storage, installer/updater, and CI.
  - Require a credible filesystem/network sandbox or a clearly constrained alternative;
    an unsandboxed binary is preview, not production support.

- [ ] **Safe update channels**
  - Stable, beta, and nightly channels consume the signed release manifest from v1.0,
    verify provenance before replacement, and retain one known-good rollback binary.
  - Check asynchronously and notify; never auto-apply. Package-manager installs defer to
    their package manager instead of competing with it.
  - `/update` previews version, compatibility/migration notes, plugin impact, and binary
    provenance before explicit confirmation.

**Exit bar:** one session can start in the TUI, reconnect through a browser or VS Code,
receive a remotely paired steering message, continue after disconnection, and run in a
declared container/remote workspace with identical policy, budget, and evidence semantics.

### 1.4 — Ecosystem and organization readiness

**Goal:** let users and teams extend bonsai at scale without turning plugins or managed
configuration into a new execution bypass.

- [ ] **Plugin bundle format**
  - A versioned manifest packages skills, agents, hooks, MCP servers, themes, commands,
    binaries, verification profiles, and optional language/provider adapters.
  - Declare namespace, supported bonsai versions/platforms, effects, permissions, network,
    sandbox needs, batching, entry points, dependencies, update policy, and uninstall data.
  - Plugin-shipped agents cannot silently grant themselves tools, permission modes, MCP
    credentials, or hooks beyond the bundle's reviewed capabilities.

- [ ] **Secure install, lock, update, and rollback**
  - Install into a content-addressed cache after manifest/schema validation; pin exact
    versions and hashes in a lockfile; support audit, disable, update preview, rollback,
    and complete uninstall.
  - Verify signatures/provenance, show capability diffs before update, and quarantine a
    bundle whose identity or requested effects change.
  - Support organization allowlists and private registries. Do not execute directly from a
    mutable checkout or marketplace response.

- [ ] **Registry and discovery**
  - Searchable catalog for plugins, skills, agents, MCP templates, themes, provider
    connections, and eval packs with compatibility, provenance, permissions, maintenance,
    and security status.
  - Separate discovery metadata from installation authority; popularity is never a trust
    signal and remote descriptions remain untrusted.

- [ ] **Provider and extension conformance kits**
  - Golden request/stream/error/usage/reasoning/cache/vision fixtures for transports and a
    CLI that validates a new catalog connection without changing bonsai source.
  - MCP/hook/plugin fixtures validate lifecycle, timeout, cancellation, batching,
    untrusted output, authorization, sandbox, resume, and failure degradation.
  - Publish a compatibility report so “supported” means tested behavior, not a name in a
    picker.

- [ ] **Full MCP surface**
  - Add resources, prompts, roots, completion, notifications, and elicitation with the same
    namespace, trust, timeout, cancellation, and status model as MCP tools.
  - Treat server-requested sampling, credential challenges, and user elicitation as new
    effects with explicit provider spend and permission boundaries, not callbacks that can
    silently escape the agent policy.
  - Negotiate capabilities per server and degrade unsupported or failed features without
    taking unrelated servers or built-ins down.

- [ ] **Provider capability expansion**
  - Add Gemini only through a native transport once the conformance suite proves it cannot
    fit an existing protocol; keep same-transport vendors declarative.
  - Model chat, responses, embeddings, reranking, vision, structured output, prompt cache,
    reasoning, and tool-streaming as explicit capabilities rather than provider-name tests.
  - Publish tested capability matrices and graceful fallbacks for local and hosted models.

- [ ] **Managed policy and enterprise environments**
  - Signed organization policy for allowed providers/models, autonomy ceilings, network
    domains, MCP/plugins, hooks, sandboxes, data retention, logging/export, and remote
    control.
  - Support proxies, custom certificate authorities, air-gapped catalogs, private model
    gateways, zero-retention modes, and admin-disabled features without degrading local
    error messages.
  - Policy can restrict but never silently broaden a user's or repository's permissions.

- [ ] **Credential broker and secret references**
  - Resolve short-lived credentials from OS stores, OIDC, vaults, or organization brokers
    at the last responsible moment; pass handles/references through task state.
  - Record issuer, scope, expiry, and use without recording the value. Revoke on session,
    worktree, remote-run, or plugin teardown.

- [ ] **Team handoff and shared evidence**
  - Transfer a session/task with immutable transcript/effect/check evidence, branch and
    worktree identity, remaining budget, and explicit ownership.
  - Share redacted read-only replays and review artifacts; collaborative writes use leases,
    roles, and the same approval policy rather than last-write-wins chat.
  - Project memories, feature records, skills, and policies can be team-shared, while user
    preferences and credentials remain private.

**Exit bar:** an organization can approve a signed private plugin, distribute constrained
policy and short-lived credentials, run it offline or behind a proxy, transfer a task to
another developer, and independently verify every extension effect and task artifact.

### 2.0 — Distributed engineering agent platform

**Goal:** coordinate durable, reviewable software work across repositories and compute
environments while retaining bonsai's provider independence and local-first option.

- [ ] **Durable scheduler and runner protocol**
  - Queue, lease, heartbeat, retry, cancel, prioritize, and resume jobs across local,
    self-hosted, and ephemeral runners with exactly-once external-write semantics.
  - Reproducible source checkout, worktree/container isolation, content-addressed caches,
    artifact retention, resource quotas, health checks, and automatic runner cleanup.

- [ ] **Cross-repository planning and delivery**
  - Model dependency order, compatibility constraints, rollout, migrations, generated
    clients, and coordinated releases across repositories.
  - Decompose an epic into repo-scoped task DAGs and PRs, but keep merge/deploy approval at
    explicit human or organization policy gates.

- [ ] **Engineering work queue**
  - Ingest approved issues, incidents, dependency alerts, flaky tests, CI failures, and
    review requests; deduplicate, prioritize, estimate, and route them to appropriate
    skills/models/environments.
  - Every item carries provenance, business/user priority supplied by humans, budget,
    policy, owner, current evidence, and a reversible terminal state.

- [ ] **Evidence-based change control**
  - Treat patches, builds, tests, browser runs, security scans, reviews, approvals,
    deployments, and rollback plans as signed artifacts attached to a change.
  - Support human review queues, separation of duties, required checks, exception records,
    and export to existing forge/CI/change-management systems.

- [ ] **Fleet economics and quality routing**
  - Route by task risk, language, privacy, eval history, latency, availability, and budget;
    use fallback, consensus, or specialist fleets only when expected quality justifies cost.
  - Continuously evaluate production task classes with privacy-preserving outcome signals;
    never train on user code or prompts without explicit opt-in and governance.

- [ ] **Hybrid local/self-hosted service**
  - A developer can keep execution and data local while using a shared scheduler, or run
    the entire control plane and provider gateway inside an organization.
  - Document tenancy, authentication, authorization, encryption, backup, disaster recovery,
    upgrades, observability, and service-level objectives before offering hosted mode.

**Exit bar:** from an approved multi-repository engineering objective, bonsai can create a
policy- and budget-bounded task graph, execute it on isolated runners, produce coordinated
reviewable PRs and signed evidence, recover from runner/provider failure, and stop before
merge/deploy for the required human decisions.

### Research horizon — promote only with evidence

These are worthwhile experiments, not commitments to ship. Promote one into a numbered
release only when dogfooding or evals show a repeated user problem, the security boundary
is understood, and the feature improves outcomes enough to justify its prompt/schema,
runtime, maintenance, and support cost.

- [ ] **Automatic model routing, fallback, and best-of-N** — optimize correctness, cost,
  latency, privacy, and availability jointly; explicit user selections always win.
- [ ] **Learned patterns and adaptive memory** — infer conventions with confidence,
  provenance, decay, contradiction handling, and accept/reject controls.
- [ ] **Formal and generative verification** — property-test generation, fuzzing, mutation
  testing, model checking, symbolic execution, and proof assistants through skills/plugins
  before core integration.
- [ ] **Dependency maintenance agent** — advisory intake, compatibility research, minimal
  upgrades, changelog/migration analysis, security review, and staged rollout evidence.
- [ ] **Production investigation agent** — correlate logs, traces, metrics, deploys, source,
  and runbooks through read-only connectors; remediation remains separately authorized.
- [ ] **Richer computer-use and visual development** — native/simulator automation,
  design-system inspection, visual regression, and accessibility testing beyond browsers.
- [ ] **Additional artifact workflows** — notebooks, PDFs, diagrams, generated docs, data
  files, and release notes as plugin-provided typed artifacts.
- [ ] **Project-context maintenance** — propose updates to steering files, feature records,
  architecture decisions, and runbooks after structural changes, with normal review.
- [ ] **Ambient channels** — notifications and carefully scoped steering from chat/mobile;
  no channel may silently become an authority to mutate code or external systems.

---

## Scope decisions

These decisions remove attractive distractions from the 1.0 path:

- **Remove product-roadmap management and `/roadmap` from the product plan.** bonsai should
  become excellent at changing software before becoming a general product-management
  system. Revisit only as a user-authored skill or plugin.
- **Do not put server/web/desktop/SDK work on the 1.0 critical path.** The supported 1.0
  products are TUI and headless/CI.
- **Do not restore `/rewind`/`/redo` as a standalone 1.0 subsystem.** Ship worktree-based
  recovery for autonomous runs first; revisit checkpoints in 1.1 with bash-aware scope.
- **Do not auto-route models before evals prove the policy.** Existing explicit model and
  effort shortcuts remain the predictable control plane.
- **Do not optimize persistence hashing, background output cloning, remote token-count
  policy, or every provider tokenizer without profiling evidence.** Track the metrics and
  promote only measured bottlenecks.
- **Drop idle-agent “busy work.”** Waiting agents should consume no provider budget unless
  the user explicitly delegated another useful task.
- **Keep the model-visible tool surface lean.** Add a tool only when structured output or
  policy removes repeated turns; prefer skills, subagents, commands, and MCP for optional
  workflows.
- **Fold `/whoami` into provider status and `/doctor`; do not add `/think` while model
  effort controls and shortcuts already exist.** `/usage` is already shipped. `/edit`, vim
  mode, desktop notifications, and `/btw` remain 1.x polish rather than release blockers.

---

## Planned command surface

Only commands tied to open work are listed here; shipped commands stay documented in
`README.md`.

| Command | Purpose | Target |
|---|---|---|
| `/batch <goal>` | Plan and run approved parallel units in isolated worktrees. | 1.1 |
| `bonsai work issue <id>` | Deliver an issue as a verified draft PR. | 1.1 |
| `bonsai review --pr <n>` | Run verified inline PR review. | 1.1 |
| `/triggers` | Validate, inspect, enable, disable, and run event triggers. | 1.1 |
| `/branch` / `/rewind` | Fork or selectively restore a checkpoint with an honest workspace boundary. | 1.1 |
| `/knowledge` | Register, refresh, inspect, and remove durable knowledge sources. | 1.2 |
| `/index` | Inspect/rebuild project graph and retrieval health. | 1.2 |
| `bonsai serve` | Start the authenticated local session API. | 1.3 |
| `/remote-control` | Pair a selected local session with another client/device. | 1.3 |
| `/update` | Preview and apply a verified bonsai update when not package-managed. | 1.3 |
| `/plugins` | Discover, audit, install, lock, update, disable, and remove bundles. | 1.4 |
| `/policy` | Explain effective organization/user/project restrictions and provenance. | 1.4 |

---

## Continuous release discipline

- Keep `master` green with formatting, clippy warnings denied, locked tests, and a warning-
  free release build.
- Every behavior change gets inline unit/integration coverage; every TUI surface gets
  reducer/render coverage; every release path gets a real-binary smoke test.
- Record eval score, tokens, cost, latency, cache reuse, repair turns, and failure class by
  provider/model/effort. Optimize from measured task outcomes, not anecdotes.
- Treat web, MCP, repository files, hook output, peer messages, and external process output
  as untrusted data across persistence, compaction, resume, and export.
- Update this file in the same change that alters scope or ordering. Remove completed work;
  do not turn the roadmap back into a changelog.
