---
name: javascript-writer
description: JavaScript correctness and idiomatic style — package-manager detection, the lint/test verify loop, ESM/CJS pitfalls, async and equality footguns, and clean modern conventions. Load before writing, reviewing, or refactoring JavaScript.
activation:
  extensions: [js, mjs, cjs, jsx]
---

# JavaScript Writer

How to write JavaScript that is correct *and* idiomatic: use the project's own
toolchain, verify with the linter and tests early, avoid the language's
well-known footguns (loose equality, `var`, floating promises), and hold the
code to the standard of a well-reviewed package. (If the project has a
`tsconfig.json`, the TypeScript guidance applies too.)

## The verify loop

JavaScript has no compiler, so ESLint and the tests are your fastest feedback.
First **detect the package manager** from the lockfile — `package-lock.json` →
npm, `pnpm-lock.yaml` → pnpm, `yarn.lock` → yarn, `bun.lockb` → bun — and use
it for every command. Then run in tightening circles:

1. `npx eslint path` (or the project's `lint` script) after each substantive
   edit — ESLint catches undefined vars, unused code, floating promises, and
   many real bugs, not just style.
2. Tests nearest the change first: `node --test` for the built-in runner, or the
   project's runner (`vitest run path`, `jest path -t 'name'`). Then the full
   suite via `npm test` / `pnpm test`.
3. Format last with Prettier (`npx prettier -w path`) so style churn never mixes
   into review.
4. If a `tsconfig.json` (or JSDoc `checkJs`) is present, `npx tsc --noEmit` is
   your type check — run it before declaring done.

Never conclude a change works because it "looks right" — JavaScript will run a
`TypeError: undefined is not a function` straight into production. If lint and
the relevant test haven't passed, the change isn't done.

## Package manager and dependencies

- Add deps with the detected manager (`npm i <pkg>` / `pnpm add` / `yarn add`),
  and `-D` for dev/tooling deps; never hand-edit the `dependencies` block and
  reinstall blindly.
- Never hand-edit the lockfile; change `package.json` and let the manager
  resolve. CI runs `npm ci` (or `--frozen-lockfile`) — a stale or edited lock
  fails the install.
- Check `package.json` `"type"` and `"engines"` before choosing syntax:
  `"type": "module"` means ESM (`import`/`export`), its absence means CJS
  (`require`/`module.exports`). Mixing them is a top source of runtime errors.

## Modules: ESM vs CommonJS

- Match the file's module system. In ESM, imports are static and hoisted and
  there is no `require`/`__dirname` (use `import.meta.url`); `.mjs` forces ESM
  and `.cjs` forces CJS regardless of `package.json`.
- Prefer named exports over a default export for anything with more than one
  meaningful symbol — named exports are greppable and refactor-safe.
- Keep modules small and side-effect-free at import time; do work in exported
  functions, not top-level statements.

## Async correctness

- `async`/`await` over `.then()` chains and callback nesting. **Never leave a
  promise floating** — `await` it or explicitly `.catch()` it; an unhandled
  rejection can crash the process.
- Run independent async work concurrently with `Promise.all`, not a sequential
  `await` in a loop, when the iterations don't depend on each other. Use
  `Promise.allSettled` when you need every result regardless of failures.
- Every `await` is a suspension point — don't assume state you read before it is
  still valid after.

## Correctness footguns

- Use `===`/`!==` — never the coercing `==`/`!=` (`0 == ''`, `null == undefined`
  are true). Guard with explicit checks (`x == null` is the one accepted use, to
  catch both `null` and `undefined`).
- `const` by default, `let` only when reassigned, **never `var`** (function
  scope + hoisting cause bugs). Prefer immutable updates (spread, `map`/`filter`)
  over mutating shared arrays/objects.
- Optional chaining `?.` and nullish coalescing `??` over `&&`/`||` ladders —
  `||` treats `0`/`''`/`false` as absent, `??` only catches `null`/`undefined`.
- Beware `JSON.parse` on untrusted input, `NaN` from bad arithmetic
  (`Number.isNaN`), and floating-point money math.

## Idiomatic style and conventions

- Casing: `camelCase` variables/functions, `PascalCase` classes/components,
  `SCREAMING_SNAKE_CASE` module constants. Functions are verbs, booleans read as
  questions (`isReady`, `hasItems`).
- Prefer array methods (`map`/`filter`/`reduce`/`find`/`some`) where they read
  clearly; a `for...of` loop when side effects dominate. Small pure functions;
  early returns to keep the happy path flat.
- Fix ESLint findings at the root rather than `// eslint-disable`-ing them — the
  rule usually points at a real problem. Don't ship `console.log`; use the
  project's logger.
- New tests sit beside the code (`foo.test.js`) or under `test/`, matching the
  project's runner and layout; match the surrounding code's conventions —
  quotes, semicolons, module style — over any external ideal.
