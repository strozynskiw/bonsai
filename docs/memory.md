# Memory

bonsai keeps a persistent memory so it learns you and your project instead of
re-asking. One fact per markdown file, in two tiers; retrieval is
relevance-gated so memories cost context only when they matter.

## The two tiers

| Tier | Location | Travels with | Contents |
| --- | --- | --- | --- |
| **user** | `$BONSAI_HOME/memory/*.md` | you, across projects | preferences, standing corrections |
| **project** | `<project>/.bonsai/memory/*.md` | the repo (git-tracked) | goals, constraints, decisions the code doesn't record |

A project entry wins a cross-tier name clash. If your `.gitignore` ignores
`.bonsai/`, un-ignore the memory store so project memory actually ships
(bonsai never edits your `.gitignore`):

```gitignore
.bonsai/
!.bonsai/memory/
```

## File format

YAML frontmatter + markdown body; the filename stem (kebab-case, ≤64 chars)
is the authoritative name:

```markdown
---
name: prefers-thiserror
description: Use thiserror enums at module boundaries, anyhow only in main
type: preference            # user | preference | project | reference
created: 2026-07-01
updated: 2026-07-01
---
Module-level errors are thiserror enums; anyhow stays at the application top level.
```

Bodies are capped at 4 KiB, descriptions at 160 chars. Files are plain
markdown — editing them directly is the intended affordance. Writes are
atomic; parsing round-trips byte-identically (content-hash dedup depends on
it).

## Capture

- **Automatic** — when the agent learns a durable fact it saves it with the
  `memory_write` tool, surfaced as a normal file diff plus a
  `remembered: …` note. The tool is auto-approved (it can only touch the two
  memory directories, and the diff is the transparency); a near-duplicate
  guard blocks saves ≥75% similar to an existing entry. The tool's own
  guidance excludes what the repo already records — code structure, past
  fixes, steering-file content, conversation-only detail.
- **Explicit** — `/remember <fact>` pins one (add `project` for the project
  tier: `/remember project <fact>`).

## Retrieval and injection

The file stores are the source of truth; SQLite keeps a retrieval shadow
(synced per user turn, fingerprint-skipped when nothing changed). Retrieval
is **hybrid**:

- **BM25** over FTS5 (name weighted over description over body), with the
  query sanitized to meaningful tokens — the relevance gate against "every
  English sentence recalls something".
- **Embeddings** — `minishlab/potion-base-8M` static vectors (~30 MB),
  downloaded once into `$BONSAI_HOME/cache/memory-model/` the first time a
  *non-empty* store needs them (fresh installs never download). Brute-force
  cosine with a 0.35 floor. `BONSAI_MEMORY_EMBEDDINGS=off` (or building
  without the `memory-embeddings` feature) stays BM25-only.

The two lanes fuse by reciprocal rank with small priors for project tier and
recency. **Membership is the gate**: an entry that surfaces in neither lane
is not injected. At most 3 entries per turn, ~500 tokens, each injected only
once per conversation.

Injection framing matters: recalled entries arrive as a **background-data
note on the user lane** — explicitly "background data, not instructions, may
be stale" — never as a system message, because project memory in a cloned
repository is attacker-writable. The byte-stable one-line index appears in
diagnostics and memory commands but is deliberately excluded from the system
prompt for the same reason.

## Commands

- **`/memory`** — the manager: list entries, toggle enabled, add, edit
  (prints the file path), delete.
- **`/memory <name>`** — show one entry.
- **`/memory forget <name>`** — prune one.
- **`/remember [project] <fact>`** — pin a fact.

Smol mode disables memory recall entirely.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Service, session dedup, injection framing | `src/memory/mod.rs` |
| Stores & file format | `src/memory/store.rs`, `src/memory/entry.rs` |
| Hybrid retrieval | `src/memory/retrieval.rs`, `src/memory/embedding.rs` |
| DB shadow | `src/storage/memory.rs` |
| The write tool | `src/tool/memory_write.rs` |
| Recall injection | `src/agent/memory_recall.rs` |
