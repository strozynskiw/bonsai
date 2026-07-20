# Troubleshooting

Start with `bonsai doctor`. It checks the release-critical local runtime
without contacting a provider, and every warning or failure includes a
concrete next action.

## `bonsai doctor` / `/doctor`

Run `bonsai doctor` outside the TUI or `/doctor` inside it. Checks, in
order:

| Check | What it verifies |
| --- | --- |
| `database` | migrations applied, `PRAGMA quick_check`, write access |
| `model_catalog` | catalog loads cleanly and has connections |
| `config` | layered config parsed; diagnostics surface here |
| `provider_auth` | current provider exists, is authorized, has a model selected |
| `provider_reachability` | *online only*: bounded read-only discovery probe |
| `sandbox` | backend present **and enforcing**: a control write inside a writable root must succeed, a write outside every root must be blocked, network denial verified |
| `git`, `github_cli` | `git` / `gh` on PATH |
| `language_server` | the workspace's relevant LSP command is installed |
| `mcp` | configured/enabled server counts |
| `mcp_reachability` | *online only*: probes enabled servers |
| `terminal` | native PTY allocation |
| `release` | offline: embedded release identity; online: signed-manifest verification and update state |

- `bonsai doctor --json` / `/doctor json` emit a versioned,
  credential-redacted machine-readable report; the CLI exits non-zero when a
  required check fails.
- The default is fully offline. `--online` (or `/doctor online`) adds
  bounded, read-only probes of the selected provider, every enabled MCP
  server, and the signed release state. Probes report only service names,
  versions, and pass/fail — no bodies, credentials, URLs, or local paths.

## Common problems

**Config edits don't apply.** Run `/config validate` — it re-parses both
layers from disk and prints diagnostics naming the entry and field. One
malformed entry never hides healthy ones. Most config changes take effect on
relaunch.

**A model can't start.** Reopen `/authorize` and `/model`. Local compatible
servers also need their base URL and model id to match what the server
actually serves (`/providers edit <id>`). `/refresh` re-pulls catalogs and
provider listings.

**Everything prompts / nothing runs in CI.** Headless denies anything that
would prompt. Raise autonomy explicitly (`--autonomy balanced`,
`BONSAI_AUTONOMY`), or add project allow rules interactively first. Project
config/MCP/hooks/skills stay inert until the workspace was trusted in an
interactive session — see [Security model](security.md#workspace-trust).

**Sandbox unavailable.** `/sandbox` reports the missing backend and next
action (macOS needs `/usr/bin/sandbox-exec`; Linux needs `bwrap` *and*
working namespace creation — containers often block it). Autonomy and
permission rules still apply, but no OS confinement is claimed, and
`auto-accept` degrades to `balanced` behavior.

**A hook shows "awaiting trust approval".** Project shell/http hooks need a
one-time approval; answer the prompt, or check `/hooks`. Editing the hook's
command/URL re-prompts by design.

**An MCP server shows failed or degraded.** `/mcp` shows state and
provenance; `/mcp reload <server>` reconnects; a failed OAuth refresh points
back to `/mcp auth <server>`. A tool-name collision degrades the server
visibly — rename one side.

**An isolated run's changes are "missing".** They're in the recovery
worktree: `bonsai recovery list`, `inspect <id>`, then `merge` or `keep`.
`merge` refuses if the source changed since the run started — use `keep` and
integrate manually. See [Recovery](recovery.md).

**Session won't resume.** A *live* session (another process heartbeating it)
refuses resume; check `/peers`. Cross-project ids refuse too. Crash
leftovers appear as *interrupted* and resume normally.

**Where are the logs?** Interactive and headless sessions log to
`$BONSAI_HOME/logs/bonsai-<ts>-<pid>.log` (newest 10 kept); `BONSAI_LOG` or
`RUST_LOG` raise verbosity. `BONSAI_TRANSCRIPT_LOG=<path>` dumps full
request/response traffic (large; debugging only). Headless stderr stays
warn-only so script output remains clean.

**Upgrade went wrong.** Every migration is preceded by an automatic snapshot
in `$BONSAI_HOME/backups/`; a failed migration restores it automatically.
Manual backup/rollback procedure: [Compatibility](compatibility.md).

## Support checklist

1. `bonsai doctor --json` (redacted, machine-readable).
2. `/config validate` output if config-related.
3. The `$BONSAI_HOME/logs/` file covering the session.
4. For provider issues: provider id, model, and whether `--online` doctor
   reachability passes.
