# Recovery worktrees

Recovery worktrees are bonsai's Git isolation boundary: a mutating run
executes in a **detached managed worktree** seeded from an exact snapshot of
the source checkout, so the checkout you launched from is never modified
mid-run. When the run finishes you review the result and decide — merge it
back, keep it as a branch, or discard it.

## When isolation applies

- **Headless (`bonsai -p`)** — governed by `--isolation auto|worktree|off`.
  `auto` (the default) isolates mutating runs at `balanced`, `auto-accept`,
  and `yolo` autonomy. An automatic mutating run **outside Git refuses to
  start** unless `--isolation off` explicitly opts out of recovery.
- **Interactive (TUI)** — opt-in per launch with `bonsai --isolation worktree`
  (also with `-c` resume). The TUI reads and edits the managed worktree, but
  session history, permissions, plans, peer identity, `/clear`, and `/resume`
  stay attached to the original project. The recovery id is visible in the
  transcript and printed again after the TUI exits. `--isolation auto` follows
  the saved autonomy level; omitting the flag leaves interactive sessions in
  the source checkout.

The snapshot includes tracked, staged, unstaged, and untracked non-ignored
files, so the run starts from what you actually had, not from `HEAD`.

Recovery setup requires the workspace to have been **trusted** during an
interactive launch — internal Git snapshot operations must never activate
repository filters or hooks before that trust decision. See
[Security model](security.md#workspace-trust).

## The recovery CLI

When an isolated run finishes, bonsai prints a recovery id:

```sh
bonsai recovery list            # every managed result
bonsai recovery inspect <id>    # what changed, against what base
bonsai recovery merge <id>      # apply to the source working tree
bonsai recovery keep <id> [branch]   # preserve as a branch
bonsai recovery discard <id>    # drop it
```

### Merge semantics

`merge` applies the reviewed result to the source **working tree** without
changing its Git index. It refuses if either the source content or the index
changed after the run started — a stale merge never silently overwrites
concurrent work. When `merge` refuses, use `keep` to land the result on a
branch and integrate it manually.

## Relationship to other safety layers

Isolation is one of three independent boundaries; it does not replace the
other two:

| Boundary | Question it answers |
| --- | --- |
| [Autonomy & permissions](autonomy-and-permissions.md) | May this action run without asking? |
| [Sandbox](sandbox.md) | What can the spawned command physically touch? |
| Recovery worktree | Can the run's file effects be reviewed before they land? |

A sandboxed, permission-gated run inside a recovery worktree is the highest
default posture for unattended work — this is exactly what headless `auto`
isolation at `balanced`+ gives you.

See also: [Headless mode](headless.md) for the flags,
[Troubleshooting](troubleshooting.md) for inspecting a run that went wrong.
