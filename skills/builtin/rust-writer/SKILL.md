---
name: rust-writer
description: Rust correctness and idiomatic style — the cargo verify loop, compiler-error recovery, ownership discipline, error-handling design, API and naming conventions. Load before writing, reviewing, or refactoring Rust code.
activation:
  markers: [Cargo.toml, rust-toolchain.toml]
  extensions: [rs]
---

# Rust Writer

How to write Rust that is correct *and* idiomatic: verify with the compiler
early and often, use cargo for every project operation, lean on the type
system to make invalid states unrepresentable, and hold the code to the
standard of a well-reviewed crate.

## The verify loop

The compiler is the fastest reviewer you have. Use it in tightening circles:

1. `cargo check` — after every substantive edit. Fast type/borrow feedback,
   no codegen. In a workspace, scope it: `cargo check -p <crate>`.
2. `cargo clippy --all-targets -- -D warnings` — before declaring a change
   done. Many projects treat warnings as errors in CI; match that bar locally.
3. `cargo test` — run the tests nearest the change first
   (`cargo test <filter>` matches test names by substring;
   `-- --nocapture` shows println output). Then the full suite.
4. `cargo fmt --all` — last, so formatting churn never mixes into review of
   real changes.

Never conclude a change works because it "looks right" — if `cargo check` has
not passed, the change is not done. Prefer several small compile-clean
increments over one large edit followed by a long error-fixing session.

## Reading compiler errors

- Fix the **first** error, then re-run `cargo check`. Later errors are often
  cascades of the first one; fixing them in place wastes edits.
- Read the *whole* diagnostic: rustc's `help:` and `note:` lines usually
  contain the exact fix (a missing bound, a suggested borrow, the right
  method name).
- `rustc --explain E0502` (any error code) gives the full explanation when a
  diagnostic is unclear.
- A type error at a call site is often a wrong change at the definition.
  Before contorting the caller, re-check the signature you just edited.

## Changing signatures and types

Rust is globally checked: a signature change breaks every caller at compile
time, not at runtime. Work with that, not against it:

- Before changing a function signature, trait, or public type, find all
  callers/implementors first (grep or symbol search), so you know the blast
  radius and update call sites in the same pass.
- When adding a field to a struct with struct-literal construction sites,
  every literal must be updated — search for `TypeName {`.
- Exhaustive `match` on an enum breaks when you add a variant. That is the
  point: visit each non-compiling match and decide the right arm; do not
  reach for a `_ =>` catch-all to make the error go away.

## Dependencies and the workspace

- Add dependencies with `cargo add <crate>` (and `cargo add <crate> --features
  <f>`), not by hand-picking versions into `Cargo.toml`.
- Never hand-edit `Cargo.lock`. Update one dep with
  `cargo update -p <crate>`; CI builds with `--locked` will fail on a stale or
  hand-edited lockfile.
- Check for a `rust-toolchain.toml` / `rust-toolchain` pin before assuming a
  toolchain feature is available; the project's edition is in `Cargo.toml`.
- In a workspace, `cargo build`/`test` at the root covers all members; use
  `-p <member>` to scope. Feature-gated code needs `--all-features` (or the
  specific `--features`) to be compiled and checked at all — code behind an
  unactivated feature is invisible to `cargo check`.

## Borrow checker: restructure, don't silence

- Do not add `clone()` just to make a borrow error compile. Prefer: narrow
  the borrow's scope (extract a value before the mutation), split a struct
  method borrow into field borrows, or restructure the data flow.
- If two mutable accesses genuinely overlap, that is a design signal — split
  the function or pass the specific fields rather than `&mut self`.
- Shared ownership (`Rc`/`Arc`) and interior mutability (`RefCell`/`Mutex`)
  are deliberate architecture choices, not borrow-error escape hatches.

## Error handling

- Library/module code returns `Result` and propagates with `?`; no
  `unwrap()`/`expect()` outside tests and genuinely unreachable states.
- `thiserror` for typed errors at module/API boundaries; `anyhow` at the
  application top level. Preserve the source error (`#[from]`, `.context()`)
  — never stringify away the cause.
- Panics are for invariant violations only; destructors (`Drop`) must never
  panic.

## Async correctness (tokio et al.)

- Never hold a `std::sync::Mutex` guard (or any non-async lock) across an
  `.await` — it can deadlock the runtime. Drop the guard first or use the
  async variant.
- Blocking calls (file IO helpers, heavy CPU work) inside async fns stall the
  executor: use the async equivalents or `spawn_blocking`.
- Cancellation is a normal path: futures can be dropped at any `.await`.
  Don't leave state half-mutated across await points that must be atomic.

## Type design: make invalid states unrepresentable

- Newtypes over raw primitives with implicit meaning (`UserId(u64)`, not a
  bare `u64` that could be any id); enums over boolean flags
  (`enum Security { Encrypted, Plain }`, not `secure: bool`).
- `Option<T>` over sentinel values (`-1`, empty string); exhaustive `match`
  over enums so adding a variant is a compile error at every decision point.
- Accept flexible argument types: `&str` over `String`, `&[T]` over `Vec<T>`,
  `&Path` over `PathBuf`, `impl Into<String>` when you genuinely store it.
  Take ownership only when the function keeps the value.
- Builder pattern for non-trivial construction; constructors are `fn new()`
  (or a descriptive `from_*`), never half-initialized structs patched later.
- Keep struct fields private by default; `pub(crate)` before `pub`; expose
  intent-revealing methods rather than raw data.

## Naming and API conventions (the community standard)

- Casing: `UpperCamelCase` types/traits/variants, `snake_case`
  functions/variables, `SCREAMING_SNAKE_CASE` constants.
- Conversions: `as_*` cheap borrow, `to_*` expensive/allocating, `into_*`
  consuming. Getters have no `get_` prefix (`fn name(&self) -> &str`);
  predicates read as questions (`is_`, `has_`, `can_`).
- Iterators: `iter()` → `&T`, `iter_mut()` → `&mut T`, `into_iter()` → `T`.
- Eagerly derive the common traits your type can support — `Debug` always,
  then `Clone`, `Copy`, `PartialEq`/`Eq`, `Hash`, `Default` as applicable —
  and implement `Display`/`From` instead of ad-hoc `to_string`/conversion
  methods.
- Document public items with `///`, including `# Errors` (when returning
  `Result`), `# Panics`, and `# Safety` (for `unsafe`) sections.

## Idiomatic style

- Early returns and `?` keep the happy path un-nested; `if let`/`let else`
  over one-armed matches.
- Prefer iterator chains (`filter`/`map`/`sum`/`any`/`find`) where they read
  clearly; use a `for` loop when side effects dominate. Don't collect into a
  `Vec` just to iterate again; `with_capacity` when the size is known.
- Immutable by default — introduce `mut` only where mutation is the point.
- `unsafe` is a last resort: isolate it in a tiny module with a `# Safety`
  contract, never sprinkle it inline.
- Fix clippy findings at the root instead of `#[allow]`-ing them; the lint is
  usually pointing at a real design smell.
- New tests live in an inline `#[cfg(test)] mod tests` next to the code
  unless the project's layout says otherwise; async tests need the runtime
  attribute (`#[tokio::test]`). Match the surrounding code's conventions —
  comment density, module layout, error style — over any external ideal.
