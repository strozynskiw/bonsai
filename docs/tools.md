# Tools

Complete reference for the built-in tools the model can call. Tools are the
agent's only way to act; every tool declares a static **effect policy** and a
**parallel policy**, and mutating tools authorize through the same permission
engine regardless of which tool fired.

## Concepts

**Effect policy** — the authorization boundary, declared per tool (a new tool
cannot compile without one):

| Policy | Meaning |
| --- | --- |
| `ReadOnly` | No side effects beyond reading; never prompts |
| `LocalState` | Mutates only bonsai-owned state (todos, titles, memory); no external prompt |
| `SelfAuthorized` | Builds a typed action plan and authorizes it before the first side effect |
| `Delegated` | Wrapper grants nothing; the child's own tools enforce their contracts |

**Parallel batching** — non-conflicting calls in one model turn run
concurrently; conflicting calls serialize. Classification (see
`src/agent/batching.rs`):

- `ReadPath` — read-only, conflicts only on overlapping paths.
- `WritePath` — write-scoped to a path (in yolo these serialize globally).
- `GlobalWrite` — serializes against everything (shell, UI-blocking, shared
  state).
- `ParallelBash` — `bash` with `parallel:true`/`run_in_background:true`;
  batches only with other parallel bash calls.
- `Independent` — never conflicts (`git`, `project_info`, `skill`, web tools).

Path conflicts are detected by prefix overlap, not exact match. Any call with
unparseable arguments, an unknown name, or a matching blocking `PreToolUse`
hook is serialized defensively.

**Registries.** Four tool registries exist: **coding** (full set),
**planning** (read-only + plan canvas), **smol** (minimal set for cheap
side-model turns), and per-run **subagent** registries (built-in subagents
get the read-only core; custom subagents may be granted more — see
[Agents and modes](agents.md)). Tool ordering on the wire is byte-stable so
the cached prompt prefix survives restarts.

**Output.** Tool results are `Text`, `Read` (with read evidence),
`Command`, `Edit {summary, diff}`, `Image`, task/subagent markers, or one of
two context forms: `TrustedContext` (harness-authored, may become a system
message — only skills use this) and `UntrustedContext` (framed external data,
**never** promoted to instructions — web, MCP, recall). Every textual field
passes secret redaction at a single choke point before the model, the
transcript, or storage sees it.

---

## Reading and navigation

All `ReadOnly`; none prompt. Paths are confined to the project root.

### `read`
Read a file or directory. Files return line-numbered content and record
**read evidence** (digest, mtime, coverage) into the read tracker — the basis
of the read-before-write guard. Directories return listings or a tree
(`depth` 1–6).

- Params: `path` (required), `offset` (1-based), `limit` (≤1000 lines),
  `depth`.
- Caps: 30,000 chars per read; an adaptive first window (~100 requested
  lines) may expand to a small whole file; files >10 MiB are paged
  line-by-line and marked as *partial* reads (a partial read does not satisfy
  the write guard); long lines clipped at 4000 chars; directory listings
  capped at 200 entries / 300 tree entries.
- Images (≤5 MiB) return as `Image`; binary/non-UTF-8 files are rejected.

### `read_region`
Read an exact 1-based line range (`path`, `start_line`, `end_line`). Use
after `symbol_search` when a whole-file read would be too large.

### `read_symbol`
Read the body of one named symbol (`path`, `query`, optional `kind`) via the
built-in tree-sitter extractors — Rust, TypeScript/JavaScript, Python, Go.

### `grep`
Regex content search. Params: `pattern` (required), `path` (space-separated
for multiple), `glob`, `type`, `mode` (`files_with_matches` default,
`content`, `count`), `multiline`, `limit`, `context_lines` (≤20),
`max_chars` (default 30,000). Matched files are marked as read.

### `glob`
File-name pattern matching (`pattern` required, `path`, `limit`), sorted
newest-first, gitignore- and hidden-aware.

### `symbol_search`
Tree-sitter symbol definitions across the project (`query`, `kind`, `path`,
`glob`, `language`, `limit`); cheap outline of a file with `path=<file>`.
Output capped at 20,000 chars.

### LSP tools — `definition`, `references`, `hover`, `workspace_symbol`
Language-server-backed navigation at a `path`/`line`/`character` position
(`references` adds `include_declaration`). Available when a managed server is
installed; see [Language intelligence](language-intelligence.md).

### `git`
Read-only git: `op` ∈ `status | diff | log | blame | show`, with `path`,
`target`, `staged`, `stat_only`, `limit` (log ≤200). Mutation ops are
rejected — pushes and commits go through `bash` where risk classification
applies. Never conflicts in batching.

### `project_info`
Compact orientation: cwd, ecosystems, git summary, steering files, available
tools, likely commands. No parameters. Each registry binds its own instance,
so a subagent's `project_info` advertises only its own toolset.

---

## File mutation

All `SelfAuthorized` through one shared guard: path confinement,
**read-before-write** (the file must have been read this session, and be
unchanged since — digest/mtime/length), workspace write-lock acquisition,
`PreFileWrite`/`PostFileWrite` hooks, then a typed action-plan authorization.
Only `yolo` autonomy skips these checks. All return `Edit {summary, diff}`
so the TUI renders a diff card.

### `write`
Create or overwrite one file (`path`, `content`). Creates parent
directories; overwriting an existing file requires a prior read; emptying a
non-empty file is classified as a truncation effect. Replayed elision markers
(from compacted history) are rejected as content.

### `edit`
Exact-string replacement: single `old_string`/`new_string`, an `edits` array
applied atomically, or `replace_all`. Empty `new_string` deletes text. Rich
recovery paths repair near-miss matches and common argument mistakes.

### `apply_patch`
One V4A patch (`*** Begin Patch` … `*** End Patch`) creating/updating/moving/
deleting several files atomically — if any hunk fails, nothing is written.
Updated files must have been read. Serializes globally in batching.

### `rename_symbol`
LSP-backed rename at a position; applies only in-project edits and requires
every touched file to have been read and to be unchanged since.

---

## Command execution

### `bash`
Execute a shell command from the project root (or `workdir` within it) with
persistent shell state (`cd` carries over between calls).

- Params: `command` (required), `timeout` (1–900 s; defaults 45 foreground /
  900 background), `workdir`, `run_in_background`, `interactive`, `parallel`.
- **Risk-classified per command**, not per tool: each command (and every
  segment, substitution, and redirect inside it) is classified into
  read-only/low/medium/high/destructive, then gated by the current
  [autonomy level](autonomy-and-permissions.md) and permission rules, run
  under the [sandbox](sandbox.md) when active, and wrapped by
  `PreBash`/`PostBash` hooks.
- Output: in-memory cap 30,000 chars; beyond that the full output spools to
  `.bonsai/tool-output/` and the model receives a bounded preview plus a
  pointer to the spool file — output is annotated, never silently amputated.
- `run_in_background: true` returns a `bg-N` task id (see `tasks`).
  `parallel: true` allows concurrent execution but does not update the
  persistent cwd. `interactive: true` allocates a PTY and returns a `pty-N`
  id (see `terminal`). An `escape_sandbox` argument requests a one-off,
  user-approved unconfined run (foreground only; never persisted).
- `cat`/`head`/`tail`/`grep` invocations mark their targets as read, feeding
  the read-before-write guard.

### `terminal`
Control the interactive PTYs started by `bash interactive:true`:
`action` ∈ `list | read | send | resize | interrupt | stop`, with `input`
(≤8 KiB, secret-looking input refused), `append_enter`, `rows`/`cols`,
`terminal_id`. PTY output is redacted and framed as untrusted command data.
A resumed session marks previously running PTYs as lost rather than
pretending to reattach.

### `tasks`
List/wait/tail/stop background work: `bg-N` bash tasks, `pty-N` terminals,
and `sub-N` background subagents. `wait_seconds` ≤60 per call.

### `diagnostics`
Run the project's type-checker in check mode and return structured
diagnostics (`kind` ∈ `all | error | warning`, `package` for Cargo, `path`
routes to the file's language server). Self-authorized (it executes a
compiler) and sandboxed.

---

## Delegation

### `agent` (alias `task`)
Delegate a focused sub-task to a subagent; only its final conclusion returns
to the parent conversation. Params: `agent` (name), `prompt`,
`run_in_background`. Built-in subagents (`explore`, `review`, `research`,
`security-review`) are read-only; custom agents get only their
frontmatter-granted tools. Read-only delegations batch in parallel;
write-capable ones serialize. Results carry nested token usage and delegated
read evidence. Subagents cannot spawn subagents. See
[Agents and modes](agents.md).

---

## Planning, state, and interaction

All `LocalState`, serialized, unprompted.

### `todowrite`
Replace the agent's task list (`todos` array with content + status). Renders
in the TODO sidebar.

### `question`
Ask the user a multiple-choice question (`question`, `header`, `options` of
2–4 `{label, description}`, `allow_multiple`) and block for the answer. In
headless mode this fails instead of blocking.

### `set_session_title`
Set the session title (3–8 words) with an `episode_action` hint
(`new_topic` closes the current [task episode](context.md#task-episodes);
`same_topic` continues it).

### `peers`
Inter-agent coordination between concurrent bonsai sessions in the same
project: `list`, `send` (bounded, redacted, rate-limited), `wake_when_done`,
`claim`/`release` advisory claims. See [Sessions](sessions.md#peer-sessions).

### `memory_write`
Save/update/forget a durable fact in the [memory](memory.md) stores
(`action`, `tier` user/project, `type`, `name`, `summary` ≤160 chars,
`body`). Auto-approved — writes are confined to the two memory directories
and always surfaced as a diff.

### `recall`
Page archived context back in: `{episode: N}` restores an archived episode's
real messages/tool outputs (cursor-paged, ~20,000 chars/page);
`{query: …}` searches this session's transcript and archives. Recalled
content returns inside an untrusted-data frame — historical data, not
instructions.

### `skill`
Load the full instructions of a skill named in the prompt's Skills index.
Returns `TrustedContext` — skills are trusted operator/project content, the
only tool output that may be promoted to instructions. See
[Skills](skills.md).

### Plan-canvas tools (planning registry only)
Sixteen `plan_*` tools maintain the live plan canvas:
`plan_replace_draft`, `plan_patch_section`, `plan_remove_section`,
`plan_move_section`, `plan_insert_task`, `plan_update_task`,
`plan_remove_task`, `plan_check_task`, `plan_uncheck_task`,
`plan_remove_phase`, `plan_move_phase`, `plan_add_question`,
`plan_remove_question`, `plan_add_finding`, `plan_associate_finding`,
`plan_resolve_finding` (findings are kept on record, never deleted).
Phases enter via `plan_replace_draft`'s phased form.

---

## Web tools

Both return `UntrustedContext` — fetched content is framed data the model
must not treat as instructions, and the frame is escape-proofed against
delimiter smuggling. Both respect sandbox network denial.

### `webfetch` (aliases `web_fetch`, `WebFetch`)
Fetch an http(s) URL as markdown (`url`, `max_chars` 1,000–100,000, default
30,000). HTML converts via html5ever-based markdown conversion; text/JSON/XML
pass through; binary is refused; ≤5 redirects with each new domain re-checked.
Fetching a fresh domain is **High** risk: `balanced` and below prompt per
domain (allow once / session / project / deny / never); domain rules live in their own
permission namespace. SSRF protection validates resolved addresses are public.

### `websearch` (aliases `web_search`, `WebSearch`)
Search via Brave, Tavily, or SearXNG (`query` ≤400 chars, `count` ≤10,
`domains` filter ≤8). Returns bounded titles/URLs/dates/snippets. Search
never implicitly fetches result pages — the search-provider domain and each
source domain get separate network permission checks. Provider selection:
Brave → Tavily → SearXNG, or `BONSAI_WEB_SEARCH_PROVIDER` explicitly. See
[Language intelligence](language-intelligence.md#web-research) for
configuration.

---

## MCP tools (dynamic)

Each connected [MCP server](mcp.md) tool registers as
`mcp__<server>__<tool>` on the wire (dotted `mcp.<server>.<tool>` in
permission rules and hooks). MCP tools are `SelfAuthorized`; their risk tier
and batching come from the server's *user-declared* capabilities (undeclared
⇒ High risk + serialized). A remembered approval is bound to the server's
display id, capabilities, and the tool's full input schema — any change
re-prompts. Results are **always** untrusted-framed, even errors. A wire-name
collision with a built-in visibly degrades that server rather than shadowing.

---

## Batching quick reference

| Class | Tools |
| --- | --- |
| `ReadPath` | read, read_region, read_symbol, glob, grep, symbol_search, definition, references, hover, workspace_symbol; read-only `agent` delegations |
| `WritePath` | write, edit, rename_symbol (→ `GlobalWrite` in yolo) |
| `GlobalWrite` | apply_patch, bash (default), terminal, diagnostics, todowrite, set_session_title, tasks, peers, question, memory_write, recall, all `plan_*`, write-capable/unresolvable `agent`, unparseable/unknown calls, blocking-hook-matched tools |
| `ParallelBash` | bash with `parallel:true` or `run_in_background:true` |
| `Independent` | project_info, git, skill, webfetch, websearch |

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Tool trait, output types, untrusted framing | `src/tool/mod.rs` |
| One file per tool | `src/tool/<name>.rs` |
| Registry profiles & registration | `src/tool/profile.rs`, `src/bootstrap.rs` |
| Batching classification | `src/agent/batching.rs` |
| Mutation guard | `src/tool/file_mutation.rs`, `src/tool/read_tracker.rs`, `src/tool/read_evidence.rs` |
| Action plans & authorization | `src/tool/action_policy.rs`, `src/tool/risk.rs` |
| Schema/search/output helpers | `src/tool/schema.rs`, `src/tool/search.rs`, `src/tool/output.rs` |
