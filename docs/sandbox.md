# Command sandbox

The sandbox is an **enforcement floor** independent of autonomy and
permission rules: autonomy decides whether a command may run without asking;
the sandbox decides what that spawned command can physically do if it runs.
It confines **writes** to approved roots and (by default) **denies network
egress**. Reads stay broad so toolchains can inspect SDKs and system files —
protecting secret reads is [redaction](security.md#redaction)'s job.

The sandbox confines only the child command, never the bonsai process itself
(which must keep writing `~/.bonsai`). It is independent of `yolo`: enabling
yolo does not turn the sandbox off; leaving confinement is always an explicit
`/sandbox off`.

## Backends

| Platform | Backend | Mechanics |
| --- | --- | --- |
| macOS | Seatbelt | `/usr/bin/sandbox-exec -p <profile>` with a generated SBPL profile: allow default, deny `file-write*` except the writable roots (+ `/dev/null`, `/dev/tty`, pty devices), optionally deny `network*`. `sandbox-exec` execs into the shell, so process-group signals reach the real child. |
| Linux | Bubblewrap | `/usr/bin/bwrap --unshare-all --die-with-parent --ro-bind / /`, re-binding each writable root read-write; network allowed only by adding `--share-net`. A **startup probe** actually spawns a bwrap sandbox to confirm namespace creation works; a second probe checks whether network isolation is supported (containers often block it, degrading to "network deny unenforced" with a visible warning). |
| Other | none | `/sandbox on` reports that no backend is available. Autonomy and permission rules still apply, but no OS confinement is claimed. |

## Writable roots

Resolved once at startup, canonicalized and de-duplicated:

1. **The project root.**
2. **A private per-session temp directory** (a fresh `bonsai-session-*`
   tempdir), exported to children as `$TMPDIR`, `$TMP`, and `$TEMP` — so
   concurrent sessions never share scratch space. npm, XDG, pip, and Go build
   caches are redirected below it (`npm_config_cache`, `XDG_CACHE_HOME`,
   `PIP_CACHE_DIR`, `GOCACHE`).
3. **Shared dependency stores** that cannot be cleanly overlaid:
   `CARGO_HOME` (default `~/.cargo`) and an explicitly configured
   `GOMODCACHE`. This is the deliberate compatibility exception to
   per-session cache isolation — cargo and go mix immutable dependencies
   with resolver locks in one directory. **rustup stays read-only**, so
   toolchain installation requires an approved sandbox escape.
4. Extra roots from `BONSAI_SANDBOX_WRITABLE_ROOTS` (colon-separated) and
   `writable_roots` in the `[sandbox]` [config section](configuration.md)
   (unioned).

## Network policy

Denied by default. Seatbelt appends `(deny network*)`; Bubblewrap simply
omits `--share-net` from the unshared namespace. `/sandbox net on` denies,
`/sandbox net off` allows. When the Linux backend cannot isolate networking,
the status and command output say the denial is **unenforced** rather than
silently claiming confinement.

## Sandbox escapes

A confined command that legitimately needs to step outside (e.g.
`rustup update`) can request `escape_sandbox: true` on the `bash` tool. This
is a single audited step-past:

- When the command itself also needs permission, one sandbox-warning modal
  approves both the command and its escape; Bonsai never stacks a separate
  command prompt in front of it.
- It normally offers **allow once**, **allow for this session** (remembered for
  that exact working-directory + command pair), or **deny**.
  There is deliberately no "always for project" option; escapes are never
  persisted, so the floor is never permanently lowered.
- At `auto-accept` or `yolo`, an interactive exact retry can proceed
  automatically only after the same command already failed confined with a
  sandbox-shaped denial and the command classifier rates it at most medium
  risk. First attempts, high/destructive risk, and headless runs still prompt
  or fail closed.
- A denial **aborts** the command rather than silently running it confined
  (the model expects the unconfined semantics).
- Escapes are foreground-only: they cannot combine with background,
  interactive, or parallel execution.
- Headless runs deny escape prompts at every autonomy level.

The shared enabled-flag is untouched by an escape — every other command stays
confined.

## Commands and settings

- `/sandbox` or `/sandbox status` — status modal (backend, posture, writable
  roots).
- `/sandbox on` / `/sandbox off` — toggle confinement.
- `/sandbox net on` — deny network; `/sandbox net off` — allow it.
- The **Sandbox** rows in `/settings` persist confinement and network
  preferences (interactive sessions restore them at startup; headless reads
  posture from config/env only).

Environment defaults:

| Variable | Values | Default |
| --- | --- | --- |
| `BONSAI_SANDBOX` | `1/on/true/yes/enabled` vs `0/off/false/no/disabled` | on when a backend is available |
| `BONSAI_SANDBOX_NETWORK` | `deny/denied/block/off` vs `allow/allowed/on` | denied |
| `BONSAI_SANDBOX_WRITABLE_ROOTS` | colon-separated paths | empty |

Env wins over the `[sandbox]` config section, per the normal
[precedence](configuration.md#layering).

## Interactions with the rest of the system

- **auto-accept requires the sandbox.** The `auto-accept` autonomy level
  honors its high-risk ceiling only while the sandbox is active; otherwise
  it behaves as `balanced`. See
  [Autonomy and permissions](autonomy-and-permissions.md#autonomy-levels).
- **In-process network calls respect it.** WebFetch/WebSearch and HTTP hooks
  refuse to run when the sandbox denies network, recording the denial in the
  authorization ledger — the deny applies to bonsai's own outbound requests,
  not just spawned commands.
- **MCP stdio servers** spawn inside the sandbox wrapper when it is active.
- **`bonsai doctor` proves enforcement**: its sandbox check performs a
  control write inside a writable root (must succeed) and a write outside
  every root (must be blocked), plus a network-denial probe — it verifies the
  backend actually enforces, not just that it exists.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Shared sandbox handle & env defaults | `src/sandbox/mod.rs` |
| Seatbelt profile | `src/sandbox/macos.rs` |
| Bubblewrap wrapper & probes | `src/sandbox/linux.rs` |
| Writable-root policy & escapes | `src/sandbox/policy.rs` |
| Doctor enforcement probe | `src/doctor.rs` |
