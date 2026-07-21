# Autonomy and permissions

bonsai separates *what the agent may do without asking* (autonomy + permission
rules, this page) from *what a spawned command can physically do*
([sandbox](sandbox.md)) and *which persona and tools are active*
([agents and modes](agents.md)). This page covers command risk
classification, the autonomy ladder, and the permission-rule system.

## Command risk classification

Every shell command is classified into an ordered risk tier before it runs.
Classification is deliberately concentrated in one auditable file
(`src/tool/risk.rs`):

| Tier | Meaning | Examples |
| --- | --- | --- |
| `read-only` | Reversible, no side effects beyond reading | `ls -la`, `git status`, `git diff`, `git branch` (listing), `gh --version` |
| `low` | Reversible, in-project, low blast radius | `cargo test`, `npm run build`, `git add <paths>`, `git stash`, branch/tag creation, `git merge --abort` |
| `medium` | Mutating but recoverable | `cargo build --release`, `make`, `git commit`, `git fetch`/`pull`/`merge`/`rebase`, `docker` |
| `high` | Hard to undo or wide blast radius | `rm file.txt`, `git push`, `npm install`, `curl https://…`, `gh pr create` |
| `destructive` | Always-ask floor; catastrophic or irreversible | `rm -rf build/`, `git push --force`, `git reset --hard`, `git checkout <path>` (discards edits), `curl … \| sh` |

How classification works:

- Commands are tokenized (quotes, escapes, control operators) and split into
  segments across `|`, `&&`, `;`, etc. Command substitutions
  (`$(…)`, backticks, `<(…)`) are classified **recursively** (depth ≤8). The
  final tier is the **max across all segments** — a dangerous segment can't
  hide behind a safe leading command.
- **Fail closed.** A command that can't be tokenized (unbalanced quotes), an
  unknown executable, a project-local script, or a program obscured behind
  `${…}`/backticks classifies as `destructive`.
- **Wrappers are seen through**: `env`, `nice`, `timeout`, `xargs`,
  `nohup`, … classify as the wrapped program, so `timeout 5 rm -rf /` is
  still destructive. `sudo`/`doas` anywhere is destructive.
- **Interpreters are unbounded code**: `sh -c`, `python`, `node`, `perl`,
  `awk`, `eval`, `source`, `busybox`, … → destructive (version-only
  invocations excepted).
- **Pipe-into-shell** is detected by the downstream *program token*, not a
  substring — `… | sh` is destructive, `… | sha256sum` is not, and the
  spaceless `curl x|sh` is still caught.
- **Write redirects escalate, never de-escalate**: a relative target is at
  least medium, an absolute or `~` target at least high, a raw `/dev/*`
  device destructive. `rm -rf build > /tmp/log` stays destructive.
- **Sensitive reads escalate**: `cat`/`grep`/`find` targeting `.env`, `.ssh`,
  `.aws`, key material, or absolute/home paths is high. Bare `env`/
  `printenv` is high (it dumps secrets) and deliberately not allowlisted.
- Destructive **shapes** are order- and spelling-independent: `rm` with any
  recursive *and* force flags, force pushes in all spellings
  (`--force`, `--force-with-lease` variants, `+refspec`, bundled `-f`),
  `git reset --hard`, `git clean -f`, branch deletion, fork bombs, `dd`,
  `mkfs*`.
- **Git subcommands use an explicit per-form table** (`git_tier`), not
  prefixes: listing forms of `branch`/`tag`/`remote` are read-only while
  their creation/mutation forms tier as low/medium; a plain
  `git checkout <target>` or `git restore <path>` (which can discard edits)
  and unrecognized subcommands stay at the fail-closed default. The tier
  only governs prompting — an active [sandbox](sandbox.md) still confines
  the command (workspace-only writes, network denial) after auto-approval.
- WebFetch to a fresh domain is classified exactly like shell network egress:
  **high**.

## Autonomy levels

The autonomy level sets the ceiling of what auto-runs; everything above the
ceiling prompts. Set it with `/autonomy <level>` (aliases: `default`→ask,
`auto`/`accept`→auto-accept), cycle the confined levels with `Alt+M`, or use
`/yolo` as a shortcut for the top (`/yolo on`→yolo, `/yolo off`→ask,
bare `/yolo` toggles).

| Level | Auto-runs up to | Still prompts | Guardrails |
| --- | --- | --- | --- |
| `ask` | nothing | everything risky | on |
| `conservative` | read-only | low and above | on |
| `balanced` *(default)* | medium | high, destructive | on |
| `auto-accept` | high | destructive | on |
| `yolo` | everything | nothing | **off** |

Three guardrails persist at every level **except yolo**:

- **Project-path confinement** — the bash cwd/workdir is clamped to the
  project root; file tools reject paths outside it.
- **Read-before-write** — `write`/`edit` require the file to have been read
  this session and unchanged since (digest/mtime check).
- **The destructive floor** — destructive-tier actions never auto-run, even
  at `auto-accept`, even when a remembered allow rule matches.

Two deliberate asymmetries:

- The `Alt+M` cycle is `ask → conservative → balanced → auto-accept → ask`.
  It **never lands on yolo** — removing the guardrails is always an explicit
  command.
- `auto-accept` honors its high ceiling **only while the sandbox is
  active**. If confinement is unavailable or off, the effective level
  downgrades to `balanced` (with a warning), so high-risk effects don't run
  unconfined without a prompt.

The status bar shows the level: quiet at `ask`, amber as it rises, red at
`yolo`. Headless runs default to `ask`; raise explicitly with `--autonomy`,
`--yolo`, or `BONSAI_AUTONOMY` (see [Headless mode](headless.md)).

## Permission rules

When an action prompts, you choose:

| Key | Choice | Effect |
| --- | --- | --- |
| `A` / Enter | Allow once | no rule recorded |
| `S` / `M` | Always for this session | in-memory rule, lost on restart |
| `P` | Always for this project | persisted rule, survives restarts |
| `D` / Esc | Deny | the action fails |

Rules are glob patterns in **separate namespaces** so a bash pattern can
never match a web domain or an MCP tool:

| Kind | Matches | Prompt |
| --- | --- | --- |
| `bash` | each env-stripped command segment | the standard permission prompt |
| `domain` | a host (WebFetch/WebSearch) | web-domain prompt (records redirect hops) |
| `mcp` | dotted ids like `mcp.github.*` | extension-tool prompt |
| `hook` | `hook.<name>:<hash>` trust records | one-time hook trust prompt |
| `workspace_trust` | the per-project trust record | the trust question |

Evaluation order, first match wins:

1. **Hard-deny floor** — compiled rules no user rule can override:
   `rm -rf /`, `rm -rf ~`, `sudo *`, `chmod 777 *`, `dd *`, `mkfs *`, and
   `curl|wget … | sh` in spaced and spaceless forms. These deny outright, no
   prompt possible.
2. **Session rules** (newest first).
3. **Persisted rules** — project scope first, then global.
4. **Built-in defaults** — allow common read-only commands; explicitly ask on
   `rm *`, `git push/commit/reset/… *`, installs, `cargo build/run *`,
   `make *`, `docker*`.
5. Fallback: ask.

For multi-segment commands the verdict is the most restrictive across
segments: any deny → deny; else any ask → ask; else allow.

Two floors bound even an explicit allow:

- A **remembered user allow** (session/project/global) auto-approves across
  autonomy ceilings — that's its purpose — but never a destructive-tier
  action outside yolo. Built-in default allows do *not* bypass the ceiling.
- The **structural floor**: an allowlisted program can't carry a dangerous
  redirect or a pipe-into-shell past the ceiling; structure is re-checked on
  the allow path.

### Managing rules

- `/permissions` lists built-in counts, your custom rules in priority order
  with ids, web-domain rules, and the recent **authorization ledger**
  (every decision with tier, effects, source, autonomy, and sandbox
  posture).
- `/permissions remove <id>` deletes a persisted rule.
- Rules persist in SQLite per project (plus a global scope); when no database
  is available, "always for project" degrades to a session rule.

## Typed action plans (non-bash effects)

File mutations, MCP calls, web fetches, and other non-shell effects don't go
through command classification — each builds a typed **action plan** naming
its effects (workspace write, delete, truncate, multi-file write, protected
path, network, code execution, irreversible, …), each effect mapped to a risk
tier. The same verdict combiner then applies: explicit deny → deny;
remembered allow → auto-approve (destructive floor permitting); otherwise the
autonomy ceiling decides between auto-approval and a prompt. Every decision —
allowed, denied, prompted, bypassed — is recorded in the authorization
ledger and summarized in the [completion report](headless.md#the-completion-report).

## Headless behavior

Headless runs cannot answer prompts: any action that would prompt is
**denied** with a message pointing at `--autonomy`, `BONSAI_AUTONOMY`, or a
project allow rule, and the `question` tool fails instead of blocking.
Sandbox-escape prompts are denied in headless mode at every autonomy level.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Risk tiers & classifier | `src/tool/risk.rs` |
| Command tokenizing/segmentation | `src/tool/bash/command.rs` |
| Bash gate | `src/tool/bash/policy.rs` |
| Typed action plans & verdicts | `src/tool/action_policy.rs` |
| Permission rules & namespaces | `src/permissions.rs` |
| Autonomy holder | `src/yolo.rs` |
| Prompt plumbing | `src/interaction.rs` |
