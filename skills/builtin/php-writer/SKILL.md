---
name: php-writer
description: PHP correctness and idiomatic style — the Composer/PHPStan/test verify loop, strict types and typed properties, PSR standards, exception design, and clean API conventions. Load before writing, reviewing, or refactoring PHP.
activation:
  markers: [composer.json]
  extensions: [php]
---

# PHP Writer

How to write PHP that is correct *and* idiomatic: drive everything through
Composer, verify with the static analyzer and tests early, opt into strict
typing so the language catches mistakes, and hold the code to modern PSR
standards rather than legacy PHP habits.

## The verify loop

Modern PHP is far safer than its reputation once you turn the checks on. Run
them in tightening circles:

1. `php -l file.php` — a quick lint (parse-only) on a file you just edited.
2. `vendor/bin/phpstan analyse` (or `vendor/bin/psalm`) at the project's
   configured level — this is the closest thing PHP has to a compiler and
   catches most type and null mistakes before runtime. Run it before tests.
3. Tests nearest the change first: `vendor/bin/phpunit --filter testName`
   (or Pest: `vendor/bin/pest --filter=...`), then the full suite. A project
   often wraps these as `composer test`.
4. Format last: `vendor/bin/php-cs-fixer fix` or `vendor/bin/phpcbf` (PSR-12),
   so style churn never mixes into review of real changes.

Never conclude a change works because it "looks right" — run PHPStan and the
tests. If they haven't passed, the change isn't done.

## Strict types are the baseline

- Put `declare(strict_types=1);` at the top of **every** new file — it turns
  silent type coercion into a `TypeError` at the boundary, which is what you
  want. Without it, `"5abc"` quietly becomes `5`.
- Type every parameter, return, and property (`private int $count;`,
  `public function find(int $id): ?User`). Use union types (`int|string`) and
  `?T` for nullable, never an untyped `mixed` you could narrow.
- Prefer `enum` (PHP 8.1+) over class constants for closed sets; back it with a
  scalar (`enum Status: string`) when you persist it.

## Composer, autoloading, and dependencies

- Add packages with `composer require <vendor/pkg>` (and
  `composer require --dev` for tooling), never by hand-editing `composer.json`
  version constraints and hoping.
- Never hand-edit `composer.lock`; run `composer update <vendor/pkg>`. CI
  installs with `--no-dev`/from the lock — a stale or edited lock breaks the
  build.
- Autoload via PSR-4 (`composer.json` `autoload.psr-4`): one class per file, the
  namespace mirroring the directory. Run `composer dump-autoload` after adding a
  namespace root. Never `require` class files by hand.

## Type and API design

- Constructor property promotion for value objects
  (`public function __construct(private readonly int $x) {}`); mark them
  `readonly` so they can't be mutated after construction.
- Program to interfaces, inject dependencies through the constructor, and avoid
  global state (`global`, static mutable properties, singletons). This is what
  makes code testable.
- Return typed collections or small DTOs over associative arrays with implicit
  shape; an array `['name' => ..., 'age' => ...]` hides its contract from every
  caller and from PHPStan.

## Error handling

- `throw` specific `Exception`/`Error` subclasses, not bare `\Exception`;
  define a domain exception per failure mode when callers branch on it. Preserve
  the cause via the third `$previous` constructor argument — never swallow it.
- Catch the narrowest type that applies; an empty `catch` is almost always a
  bug. Use `finally` (or a `try`-with a closing call) for cleanup.
- Prefer exceptions to `false`/`null` sentinels for genuine failures; reserve
  `?T` returns for ordinary "not found".

## Naming and conventions (PSR)

- Follow PSR-12: `StudlyCaps` classes, `camelCase` methods/properties,
  `SCREAMING_SNAKE_CASE` constants; one blank line and 4-space indent. Namespaces
  and class names mirror the file path.
- Keep properties `private`/`protected`; expose intent-revealing methods.
  Booleans read as questions (`isValid`, `hasAccess`).
- Prefer `array_map`/`array_filter`/`array_reduce` and generators where they
  read clearly; a `foreach` when side effects dominate. Use `===`/`!==` — never
  the coercing `==`. Fix static-analysis findings at the root instead of
  `@phpstan-ignore`-ing them.
- New tests live under `tests/` mirroring `src/`; use PHPUnit (`#[Test]`
  attributes, data providers) or Pest and match the surrounding code's
  conventions over any external ideal.
