# Language intelligence

Three layers give the agent code understanding: built-in **tree-sitter**
parsing (offline, always available), managed **language servers** (when
installed), and **web research** for everything outside the repository.

## Tree-sitter symbols

Grammars for **Rust, TypeScript/TSX, JavaScript/JSX, Python, and Go** are
compiled into the binary — no network, no server. They power
`symbol_search`, `read_symbol`, and the startup repository map.

What the extractors index per language: functions, methods, structs/classes,
enums, traits/interfaces, impls, modules, type aliases, constants and
public/exported variables — each with kind, name, one-line signature,
position, and visibility (Rust `pub`, JS/TS exports and `#private`, Python
underscore convention, Go capitalization).

- `symbol_search` — project-wide definition search with kind/language/glob
  filters; pointing it at a single file yields a cheap outline before
  reading or editing.
- `read_symbol` — read one symbol's body by name.
- Gitignore is respected by default (`BONSAI_SYMBOL_RESPECT_GITIGNORE`).

### The repository map

At startup bonsai builds a ranked, token-budgeted outline of the repository
(~1,500 tokens): files scored by public-symbol count with entry-point
bonuses (`main.rs`, `lib.rs`, `index.ts`, `main.go`, `__init__.py`, …), up
to 12 symbols per file, 5,000-file scan cap. The output is deliberately
byte-stable (sorted, deterministic) so it lives in the **cacheable** prompt
prefix. The TUI builds it in the background and injects it before the first
turn; headless builds it eagerly. Smol mode omits it.

## Language servers

Definition, references, hover, workspace-symbol, rename, and diagnostics use
a managed server when its command is installed:

| Language | Server | Install | Override |
| --- | --- | --- | --- |
| Rust | `rust-analyzer` | `rustup component add rust-analyzer` | `BONSAI_RUST_ANALYZER` |
| TypeScript/JavaScript | `typescript-language-server --stdio` | `npm i -g typescript-language-server typescript` | `BONSAI_TYPESCRIPT_LANGUAGE_SERVER` |
| Python | `pyright-langserver --stdio` | `npm i -g pyright` or `pip install pyright` | `BONSAI_PYRIGHT_LANGSERVER` |
| Go | `gopls` | `go install golang.org/x/tools/gopls@latest` | `BONSAI_GOPLS` |

Mechanics:

- Servers spawn lazily, one client per (language, workspace root), with the
  root found by walking up to the nearest manifest (`Cargo.toml`,
  `tsconfig.json`/`package.json`, `pyproject.toml`/…, `go.mod`/`go.work`).
  Documents sync incrementally and only when content changed.
- If a server is absent, the tools return one setup hint and the
  tree-sitter/grep path stays available — nothing degrades silently.
- **Tools**: `definition`, `references`, `hover`, `workspace_symbol`
  (read-only) and `rename_symbol` (self-authorized: in-project edits only,
  every touched file must have been read and unchanged, overlaps rejected,
  guarded commit with rollback).
- **`diagnostics`** runs the project's checker: with a `path` it uses the
  file's language server (pull diagnostics, falling back to pushed ones);
  pathless Rust runs `cargo check` through the normal authorization and
  sandbox path.
- **Automatic post-edit diagnostics** (Rust): each mutation batch snapshots
  the error baseline and, after the edits, any *new* errors are injected as
  an `[Automatic diagnostics after edits]` note — framed as tool data, not
  instructions — so the agent fixes breakage it just caused without being
  asked.

## Web research

### `websearch`

Providers: **Brave Search**, **Tavily**, or a self-selected **SearXNG**
instance (its JSON format must be enabled). Configure before startup:

- `BRAVE_SEARCH_API_KEY` (or `BRAVE_API_KEY`)
- `TAVILY_API_KEY`
- `BONSAI_SEARXNG_URL=https://search.example.com`

When several are configured the order is Brave → Tavily → SearXNG;
`BONSAI_WEB_SEARCH_PROVIDER=brave|tavily|searxng` chooses explicitly.
Supports recency filters (day/week/month/year) and up-to-8 primary-domain
filters; returns bounded titles, source URLs, dates when available, and
snippets (≤10 results). Results are untrusted data; URLs are preserved in
the transcript and headless event stream. **Search never fetches a result
page implicitly** — the search-provider domain and every source domain go
through separate network permission checks. Queries containing something
that looks like a live secret are refused.

### `webfetch`

Fetches one URL as markdown (HTML converted, text/JSON/XML passed through,
binary refused) with hard security properties: public-address validation
with per-request DNS pinning (anti-rebinding), manual redirect following
(≤5) with re-validation and re-authorization per hop, per-domain permission
prompts at High risk, 5 MiB body cap, 30 s total deadline, and untrusted
framing of everything returned. Details in [Tools](tools.md#web-tools) and
[Security model](security.md#network-security).

## Background tasks and PTY terminals

The execution side of "run, debug, verify":

- **Background bash** (`run_in_background: true`) → `bg-N` tasks: piped
  output with a 30,000-char live tail, process-group cleanup, and completion
  notes injected as untrusted data when the agent next looks. `tasks`
  list/wait/tail/stop; `/tasks` (or `Alt+T`) is the human view.
- **Interactive PTYs** (`bash interactive: true`) → `pty-N`: a real PTY with
  a vt100 screen model. The `terminal` tool reads a normalized screen +
  raw tail (both redacted), sends ≤8 KiB of non-secret input, resizes,
  delivers Ctrl-C, or stops. Prompt detection (`password:`, `continue?`, a
  trailing `$`) wakes the agent when input is wanted. PTY launch retains the
  normal bash authorization/hook/sandbox policy and can't combine with
  parallel/background/escape modes.
- Both registries are process-local: a resumed session marks previously
  running tasks and terminals as lost rather than pretending to reattach.

## Where this lives in the code

| Concern | Location |
| --- | --- |
| Tree-sitter extractors | `src/symbol/` |
| Repository map | `src/tool/repo_map.rs` |
| LSP hub, clients, specs | `src/lsp/` |
| LSP tools & diagnostics | `src/tool/lsp.rs`, `src/tool/diagnostics.rs` |
| Web tools | `src/tool/webfetch.rs`, `src/tool/websearch.rs` |
| Background tasks & PTYs | `src/background.rs`, `src/terminal.rs` |
