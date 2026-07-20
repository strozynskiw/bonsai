# Compatibility and upgrades

This document is the compatibility contract for Bonsai 1.x. The repository has one
public pre-1.0 release lineage, `v0.1.0-alpha.1`; its persisted config/session shapes
are frozen under `tests/fixtures/upgrade/v0.1.0-alpha.1/` and tested on every build.

## 1.x compatibility surface

| Surface | 1.0 schema | 1.x promise |
| --- | --- | --- |
| Global/project `.bonsai/config.toml` | `schema_version = 1` | Existing v1 keys keep their meaning and type. New optional keys may be added. A breaking change requires a new schema version and migration guidance. |
| SQLite session/database state | ordered SQLx migrations | Every newer 1.x binary upgrades older 1.x state automatically. Applied migrations are immutable. User sessions and resumable state survive the upgrade. |
| Custom provider connection/model TOML | implicit format 1 | Existing connection, transport, auth, discovery, target, reasoning, and pricing fields remain accepted through 1.x. Additions are optional. |
| Skills and custom agents | implicit frontmatter format 1 | Existing documented frontmatter fields and Markdown bodies remain accepted. Unknown frontmatter keys remain ignorable; unsupported security capabilities still fail closed. |
| Hooks and MCP server config | config schema 1 | Existing documented fields and defaults retain their meaning. New capabilities or action/transport kinds are additive and opt-in. |
| MCP tool identity | `mcp__<server>__<tool>` | The wire-name mapping and dotted permission identity `mcp.<server>.<tool>` remain stable. A remote server changing its own tool name/schema is external to this promise and may trigger a new approval. |
| Headless `json` and `stream-json` | `schema_version: 1` | Every JSON object/event carries the version. Existing fields, types, status values, and event meanings are retained; consumers must tolerate new optional fields, enum values, and event types. Breaking output changes require schema 2. |

The CLI's human-readable text and TUI layout are not machine-readable compatibility
surfaces. Scripts should use `--output-format json` or `stream-json`.

## Public-alpha upgrade

On first start, Bonsai imports the `v0.1.0-alpha.1` `sessions.toml` or legacy
`config.toml`, maps the former `opencode-go` connection to `opencode`, preserves the
provider/model/theme/authorization state, moves credentials to the configured credential
store, and writes current state without plaintext secrets.

That alpha did not persist conversation transcripts in SQLite, so there is no alpha
conversation to resume. Transcript resume compatibility begins with the SQLite-backed
releases and is covered by the 1.x database promise above.

## Automatic upgrade backup and rollback

Before applying a pending SQLite migration, Bonsai creates a consistent snapshot in:

```text
$BONSAI_HOME/backups/bonsai-v<from>-before-v<to>-<timestamp>.db
```

`BONSAI_HOME` defaults to `~/.bonsai`. A failed migration closes the database, restores
that snapshot automatically, and leaves the backup in place. Successful upgrades also
retain the snapshot so the user can recover or return to the previous binary. Bonsai
does not automatically prune these backups.

Running an older binary directly against a database already upgraded by a newer binary
is unsupported. To downgrade, stop every Bonsai process and restore the pre-upgrade
snapshot created for that version.

## Manual backup and recovery

1. Stop every Bonsai process. Do not copy a live `bonsai.db` file without its WAL.
2. Copy the complete `$BONSAI_HOME` directory to a private location. This captures the
   database, config, protected-file credentials, themes, skills, and agents. Keyring
   credentials remain in the operating-system credential store; session-only credentials
   cannot be backed up.
3. Run the new Bonsai version. If startup succeeds, run `bonsai doctor`.
4. To restore, stop Bonsai, preserve the failed/current home directory for diagnosis,
   copy the saved directory back, and remove stale `bonsai.db-wal` and `bonsai.db-shm`
   files if they were not part of the same stopped-state backup.
5. Run `bonsai doctor` again before resuming work.

For a database-only rollback, replace `$BONSAI_HOME/bonsai.db` with the selected file
from `$BONSAI_HOME/backups/` while Bonsai is stopped, remove stale WAL/SHM sidecars, and
then launch the older matching binary. Config written by a newer schema loads
best-effort, but a full home-directory backup is the only supported downgrade boundary.
