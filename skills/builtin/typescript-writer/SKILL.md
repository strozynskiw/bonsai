---
name: typescript-writer
description: TypeScript correctness and idiomatic style — package-manager detection, the tsc/eslint/test verify loop, strictness and ESM/CJS pitfalls, type-driven design, and clean-code conventions. Load before writing, reviewing, or refactoring TypeScript.
activation:
  markers: [tsconfig.json]
  extensions: [ts, tsx, mts, cts]
---

# TypeScript Writer

How to write TypeScript that is correct *and* idiomatic: use the project's own
toolchain, verify with the typechecker early, let precise types carry the
design, and never trade a compile error for a runtime bug.

## Use the project's toolchain, not a generic one

- Detect the package manager from the lockfile and use it consistently:
  `package-lock.json` → npm, `yarn.lock` → yarn, `pnpm-lock.yaml` → pnpm,
  `bun.lock`/`bun.lockb` → bun. Never mix managers and never hand-edit a
  lockfile.
- Read `package.json` `"scripts"` first — the project's build, test, lint, and
  typecheck entry points are defined there. Prefer them over ad-hoc tool
  invocations; they carry the flags the project expects.
- Add dependencies through the manager CLI (`pnpm add <pkg>`, `npm install
  <pkg>`; `-D` for dev-only). Add `@types/<pkg>` when a runtime dep ships no
  types.
- In a monorepo, check for workspace config (`pnpm-workspace.yaml`,
  `"workspaces"` in package.json, turbo/nx) and run scripts in the affected
  package, not just the root.

## The verify loop

1. Typecheck after every substantive edit: the project's `typecheck` script,
   or `npx tsc --noEmit` (add `-p <dir>` to pick the right tsconfig in a
   monorepo).
2. Lint before declaring done: the `lint` script — the eslint config is
   project law, don't argue with it in the diff.
3. Test: use the runner in devDependencies — `npx vitest run <file>`,
   `npx jest <path> -t '<name>'`, or `node --test`. Run the tests nearest the
   change first, then the wider suite.
4. Types are erased at runtime — a clean typecheck does not prove behavior.
   Run the changed code path or its test before concluding it works.

## Respect tsconfig

- Check `strict` (and `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`)
  before writing: under `strictNullChecks`, `null`/`undefined` must be handled
  explicitly; with `noUncheckedIndexedAccess`, every index read is
  `T | undefined`.
- `paths` aliases, `moduleResolution`, and `target` shape what compiles and how
  imports are written — mirror the import style of neighboring files instead of
  inventing a new one.

## Type-safety discipline

- Never reach for `any` or `as unknown as T` to silence an error — that
  converts a compile-time report into a runtime bug. Use `unknown` plus
  narrowing, a type guard, or fix the actual type.
- `as` asserts, it does not check. Every cast is a place the compiler can no
  longer help; keep them rare and justified.
- Model alternatives as discriminated unions and switch exhaustively; an
  `x satisfies never` (or a `never`-typed default arm) turns a missed variant
  into a compile error.
- `satisfies` checks a value against a type without widening it — prefer it
  over an annotation when you need both checking and inference.
- Data from outside the type system — `JSON.parse`, fetch responses,
  `process.env` — is untyped at runtime. Validate at the boundary (zod/valibot
  if the project uses one) rather than blind-casting.

## ESM vs CJS

- Check `"type"` in package.json. `"module"` means ESM: `import` only, no
  `require`/`__dirname` (use `import.meta.url`), and under `NodeNext`
  resolution relative imports need an explicit `.js` extension — even when the
  source file is `.ts`.
- Default-import interop between CJS and ESM is a classic breakage: if
  `import x from 'pkg'` misbehaves, check `esModuleInterop` and try
  `import * as x`.

## Async correctness

- Every promise must be awaited, returned, or explicitly `void`-ed. A floating
  promise swallows errors and races test teardown.
- Independent async work runs through `Promise.all`; sequential `await`s in a
  loop are a latency bug unless order matters.
- `array.forEach(async …)` does not await anything — use `for … of` or
  `Promise.all(array.map(…))`.
- `try/catch` only catches a rejection if the `await` is inside the `try`;
  wrapping the call that merely *creates* the promise catches nothing.

## Type-driven design

- Give domain concepts precise types: a union of string literals
  (`type Status = 'queued' | 'running' | 'done'`) over bare `string`; a
  discriminated union over an interface of optional fields that can encode
  impossible combinations.
- Prefer string-literal unions and `as const` objects over `enum` unless the
  project already uses enums.
- `readonly` properties and `ReadonlyArray`/`readonly T[]` express intent and
  catch accidental mutation; prefer producing new values (spread, `map`) over
  mutating shared ones.
- Annotate exported/public function signatures explicitly; let inference do
  the work inside function bodies. Derive related types with `Pick`, `Omit`,
  `Partial`, `ReturnType` instead of duplicating shapes by hand.
- Interfaces vs type aliases: follow the project's convention; don't mix
  styles in one module.

## Clean-code conventions

- Naming: `camelCase` for values/functions, `PascalCase` for types/classes/
  components, `UPPER_SNAKE_CASE` for true constants. Named exports over
  default exports (better rename/refactor safety) unless the framework
  demands defaults.
- Small functions with early returns; replace boolean positional parameters
  (`doThing(true, false)`) with an options object.
- Throw `Error` (or subclasses) — never strings; a `catch (e)` receives
  `unknown` under strict settings, so narrow before using it.
- `??` vs `||`: `||` swallows `0`, `''`, and `false`. Use `??` for
  "default only when null/undefined", and `?.` for optional access —
  but don't chain `?.` past a point where absence is actually a bug.
- Prefer `undefined` over `null` for "absent" unless an API contract requires
  `null`; don't use both interchangeably in one codebase.
- Classes only when there's state plus behavior; plain functions and object
  literals are the default unit of code.
