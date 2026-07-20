---
name: ruby-writer
description: Ruby correctness and idiomatic style — the Bundler/RuboCop/test verify loop, Enumerable and block idioms, keyword arguments, exception design, and expressive conventions. Load before writing, reviewing, or refactoring Ruby.
activation:
  markers: [Gemfile, Rakefile, .ruby-version]
  extensions: [rb, rake]
---

# Ruby Writer

How to write Ruby that is correct *and* idiomatic: run everything through
Bundler, verify with the linter and tests early, express intent through the
Enumerable vocabulary and blocks, and hold the code to the standard of a
well-reviewed gem.

## The verify loop

Ruby has no compiler, so the linter and tests are your fastest feedback. Run
them in tightening circles, always through Bundler so versions match the
project:

1. `bundle exec ruby -c file.rb` — a quick syntax check on a file you just
   edited (parse-only, no execution).
2. `bundle exec rubocop path` — style + a surprising number of real bugs.
   `rubocop -A` autocorrects the safe-and-unsafe set; review the diff.
   (A project may use `standardrb` instead — check the `Gemfile`.)
3. Tests nearest the change first: `bundle exec rspec path/to/spec.rb:42`
   (RSpec) or `bundle exec rake test TEST=... -n /pattern/` (Minitest). Then
   the full suite.

Never conclude a change works because it "looks right" — Ruby will happily run a
`NoMethodError` straight into production. If the relevant spec hasn't gone
green, the change isn't done.

## Bundler and dependencies

- Add gems to the `Gemfile` (`bundle add <gem>`), then `bundle install`; never
  `gem install` into the ambient environment for a Bundler project.
- Never hand-edit `Gemfile.lock`; change a version in the `Gemfile` and run
  `bundle update <gem>`. CI installs from the lock — a hand-edit or stale lock
  breaks reproducibility.
- Respect the `.ruby-version` / `Gemfile` `ruby` pin before using a syntax
  feature; run project scripts via `bundle exec` so the right gem versions load.

## Blocks, Enumerable, and expressive iteration

- Reach for the Enumerable vocabulary over manual loops: `map`, `select`/
  `reject`, `find`, `each_with_object`, `sum`, `group_by`, `partition`,
  `flat_map`. They read as intent and avoid off-by-one mistakes.
- `&:method` symbol-to-proc for the common one-arg case (`names.map(&:upcase)`).
- Prefer `each`/`map` over `for`; use the block form of resource methods
  (`File.open(path) { |f| ... }`) so the resource closes even on exception.
- Guard clauses and `return`/`next` early to keep the happy path un-nested;
  `if`/`unless` as trailing modifiers when the body is one short line.

## Methods, arguments, and objects

- **Keyword arguments** for anything beyond one or two positional params —
  `def build(name:, size: 10)` documents call sites and survives reordering.
  Avoid the old options-hash-plus-`fetch` pattern in new code.
- Small, single-purpose methods with predicate names ending in `?`
  (`valid?`), bang names ending in `!` only for the mutating/dangerous variant
  of a safe method.
- Prefer plain objects and composition over deep inheritance; extract shared
  behavior into a module and `include` it. Freeze constants
  (`CONFIG = {...}.freeze`) so shared state can't be mutated at a distance.
- Add `# frozen_string_literal: true` at the top of new files; treat string
  literals as immutable.

## Error handling

- `raise` specific `StandardError` subclasses, not bare `RuntimeError` or
  strings; define a small exception class per failure mode when callers branch
  on it. Rescue the narrowest class that applies — never `rescue Exception`
  (it catches `Interrupt`/`SignalException`).
- Keep `begin`/`rescue` tight around the call that can fail; use the method-body
  `rescue` form for whole-method error handling. Preserve the cause — Ruby
  chains it automatically on a re-`raise`; don't stringify it away.
- Reserve exceptions for exceptional flow; return a value or `nil` for ordinary
  "not found" cases, and document which a method does.

## Naming and conventions (the community standard)

- Casing: `snake_case` methods/variables/files, `CamelCase` classes/modules,
  `SCREAMING_SNAKE_CASE` constants. Predicates end in `?`, dangerous variants in
  `!`. Files match the class they define (`user_account.rb` → `UserAccount`).
- Prefer `attr_reader`/`attr_accessor` over hand-written accessors; keep
  instance state private and expose intent-revealing methods.
- Two-space indent, no tabs; `do...end` for multi-line blocks, `{ }` for
  single-line. Let RuboCop settle the rest and fix findings at the root instead
  of `# rubocop:disable`-ing them.
- New tests live under `spec/` (RSpec) or `test/` (Minitest) mirroring the
  source layout; use `let`/`subject` and descriptive `describe`/`context`
  blocks. Match the surrounding code's conventions over any external ideal.
