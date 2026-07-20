---
name: python-writer
description: Python correctness and idiomatic style — virtualenv/uv/poetry detection, the run-and-test verify loop, typing and lint tooling, Pythonic data modeling and naming, and the pitfalls that most often break Python edits. Load before writing, reviewing, or refactoring Python.
activation:
  markers: [pyproject.toml, setup.py, requirements.txt, Pipfile, setup.cfg]
  extensions: [py]
---

# Python Writer

How to write Python that is correct *and* Pythonic: run inside the project's
environment, prove changes by executing them, model data with real types
instead of dict blobs, and follow the idioms the community reads fluently.

## Environment first

- Never install into or run against the system Python. Find the project's
  environment and go through it:
  - `uv.lock` → uv: `uv run <cmd>`, `uv add <pkg>`
  - `poetry.lock` → poetry: `poetry run <cmd>`, `poetry add <pkg>`
  - `Pipfile.lock` → pipenv: `pipenv run <cmd>`
  - a `.venv/` dir → `.venv/bin/python`, `.venv/bin/pytest` directly
  - otherwise pip + `requirements.txt`: keep the file updated when adding deps.
- Use `python3` where the environment doesn't pin a `python`; verify with
  `python3 --version` before assuming language features.
- `pyproject.toml` is the source of truth: dependencies plus configuration for
  ruff, mypy/pyright, pytest and friends live there. Read it before picking
  tools or style.

## The verify loop

- Python fails at *runtime*, not compile time: an unexecuted branch full of
  `NameError`s imports cleanly. The only proof is executing the changed path —
  run the code or its tests.
- Cheap syntax gate: `python -m py_compile <file>`; import the module to catch
  import-time errors.
- Tests: `pytest path/to/test_x.py::test_name` for one test, `-k <expr>` to
  filter, `-x` to stop at first failure. Run the nearest tests, then the suite.
- Lint/format only with what the project configures: `ruff check` / `ruff
  format` (or black/isort), and `mypy`/`pyright` when a config section exists.
  Don't impose a formatter the project doesn't use.

## Pitfalls that break edits

- Mutable default arguments (`def f(x, acc=[])`) are shared across calls —
  default to `None` and create inside.
- Closures in loops late-bind the loop variable; bind it as a default arg or
  use `functools.partial`.
- `is` compares identity — use it for `None` and sentinels only; `==` for
  values.
- Never write a bare `except:` — catch the specific exceptions, re-raise with
  bare `raise`, and don't swallow `KeyboardInterrupt`/`SystemExit`.
- Importing a module executes it: watch for side effects at module top level,
  circular imports, and never name a file after a stdlib module
  (`types.py`, `email.py` shadow the real ones).
- Use context managers (`with`) for files, locks, and connections — not manual
  close calls.
- Prefer `pathlib.Path` over string paths; pass `encoding=` explicitly when
  opening text files; keep `str` and `bytes` strictly separate.

## Typing and data modeling

- Match the project's typing discipline — fully-typed codebases expect hints
  on new code; untyped ones may not want them.
- Hints do nothing at runtime; only the configured checker (mypy/pyright)
  enforces them. An `Optional[...]` return must be handled at every call site.
- Model structured data as `@dataclass` (or the project's model library, e.g.
  pydantic) instead of raw dicts; use `Enum` instead of magic strings for
  closed sets of values.
- Give every class a useful `__repr__` (dataclasses provide one) — debugging
  quality depends on it.
- Plain attributes over Java-style `get_x()`/`set_x()`; reach for `@property`
  only when computed access must look like an attribute.

## Idiomatic style

- Naming: `snake_case` functions/variables/modules, `PascalCase` classes,
  `UPPER_SNAKE_CASE` constants, `_leading_underscore` for internal helpers.
- Iterate directly: `for item in items`, `enumerate(items)` when the index is
  needed, `zip(a, b)` for parallel iteration — never `range(len(items))`.
- Comprehensions for simple transforms
  (`[x.name for x in users if x.active]`); a full loop once logic needs more
  than one clause. Generator expressions when the result is only iterated.
- Truthiness reads naturally: `if not items:` over `if len(items) == 0:`;
  `x is None` over `x == None`.
- EAFP over LBYL: `try/except KeyError` beats checking then acting — the
  check-then-act version has a race and reads worse.
- f-strings for formatting; `logging` (not `print`) in library code; module-
  level side effects behind `if __name__ == "__main__":`.
- Small functions, early returns, explicit `return None` when a function can
  also return values; keyword-only arguments (`def f(*, force: bool)`) keep
  call sites self-documenting.
- No module-level mutable state when avoidable; pass dependencies explicitly
  over reaching for globals.

## Async

- Blocking calls (`time.sleep`, `requests`, file IO) inside `async def` stall
  the whole event loop — use the async equivalents, `asyncio.to_thread`, or an
  executor.
- One `asyncio.run(...)` at the entry point; inside async code, `await` — don't
  nest event loops.
