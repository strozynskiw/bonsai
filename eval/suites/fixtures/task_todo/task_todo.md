Build a small Rust CLI app called `tasklog` that parses a lightweight project
task file and produces useful reports.

This should be a focused parser/reporting app, not a web app. It should take
less time than a full Axum/SQLite/HTMX project, but it should still be easy to
split into multiple implementation phases.

Tech requirements:
- Rust stable, edition 2024.
- A CLI argument parser such as `clap`.
- `serde` and `serde_json` for structured output.
- `chrono` or `time` for date parsing.
- `thiserror` or `anyhow` for structured errors.
- No web server, database, async runtime, frontend framework, or Node toolchain.
- One command should run the app locally.

Input format:

The tool reads a plain-text `.tasklog` file with sections and task rows:

```text
# Project: Website Refresh
@owner alice
@timezone UTC

[Backlog]
- [ ] T-1 Write launch copy @high due:2026-07-10 #marketing
- [ ] T-2 Pick hero images @medium #design

[In Progress]
- [ ] T-3 Implement pricing page @high due:2026-07-05 #frontend

[Done]
- [x] T-4 Draft sitemap @low done:2026-07-01 #planning
```

Parsing rules:
- `# Project: ...` sets the project title.
- `@key value` lines define metadata.
- `[Section Name]` starts a task section.
- Task rows use:
  - `- [ ]` for open tasks
  - `- [x]` for completed tasks
  - an id like `T-123`
  - a free-text title
  - optional priority marker: `@low`, `@medium`, `@high`
  - optional `due:YYYY-MM-DD`
  - optional `done:YYYY-MM-DD`
  - zero or more tags like `#frontend`
- Blank lines and comment lines starting with `//` are ignored.

Core features:
- Parse a tasklog file into a typed in-memory model.
- Validate the file and report clear line/column-oriented errors when possible.
- Detect duplicate task ids.
- Reject invalid dates, invalid priorities, malformed sections, and task rows
  outside a section.
- Compute derived task status:
  - `open`
  - `done`
  - `overdue`
  - `due_soon` for open tasks due within the next 7 days
- Provide deterministic sorting for reports.

CLI commands:
- `tasklog validate <path>` validates the file and prints either `ok` or a
  readable error list.
- `tasklog summary <path>` prints a human-readable summary with counts by
  section, status, priority, and tag.
- `tasklog list <path>` prints tasks in a readable table.
- `tasklog list <path> --status open|done|overdue|due-soon`
- `tasklog list <path> --tag frontend`
- `tasklog export <path> --format json` writes the parsed model as JSON.

Suggested implementation phases:
1. Parser library:
   - Define the data model.
   - Parse project title, metadata, sections, and task rows.
   - Add unit tests for valid and invalid input.
2. Validation and reporting:
   - Add duplicate-id checks, date/priority validation, derived statuses, and
     summary calculations.
   - Add tests for edge cases and report ordering.
3. CLI integration:
   - Wire the parser into `validate`, `summary`, `list`, and `export`.
   - Add integration tests that run commands against temporary tasklog files.
   - Add a README with setup, examples, and test instructions.

Testing:
- Unit tests for task-row parsing, metadata parsing, date handling, priority
  parsing, duplicate detection, and derived status calculation.
- Integration tests for each CLI command.
- Tests should use temporary files and run with `cargo test`.

Code quality:
- Organize parser, model, validation, reporting, and CLI code into clear modules.
- Avoid `unwrap()` and `expect()` in application code.
- Keep error messages actionable and stable enough to test.
- Keep JSON output deterministic.
- Include a small README with example input and command output.

This is a compact harness task: it exercises planning, parser design, domain
modeling, validation, CLI UX, deterministic output, tests, and documentation
without requiring a web stack or persistent storage.
