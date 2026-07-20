---
name: go-writer
description: Go correctness and idiomatic style — the build/vet/test verify loop, module operations, error-handling discipline, naming and structure conventions, and the concurrency, slice, and interface pitfalls that produce wrong Go. Load before writing, reviewing, or refactoring Go code.
activation:
  markers: [go.mod, go.work]
  extensions: [go]
---

# Go Writer

How to write Go that is correct *and* idiomatic: keep the tree building and
vetting continuously, treat every error value as load-bearing, write the plain
straightforward Go the community optimizes for, and know the language traps
that compile fine and misbehave at runtime.

## The verify loop

1. `go build ./...` after every substantive edit — fast, whole-module.
2. `go vet ./...` before declaring done; it catches real bugs (printf
   mismatches, copied locks, unreachable code) that compile cleanly.
3. `go test ./path/...` nearest the change first, then the module; filter with
   `-run 'TestName'` (and `'TestName/subtest'` for `t.Run` cases). Add `-race`
   whenever the change touches goroutines or shared state.
4. `gofmt -l -w .` (or `goimports`) last — unformatted Go is a defect, not a
   style preference. Check `go.mod`'s `go` directive before using newer
   language features.

## Errors are values — handle every one

- Check every returned `err`; never discard one with `_` to make code shorter.
- Wrap with context when propagating: `fmt.Errorf("opening config: %w", err)`
  — `%w` keeps the chain inspectable via `errors.Is`/`errors.As`.
- Libraries return errors; they don't `panic`. Reserve panic for corrupted
  invariants, and `recover` only at well-defined boundaries.

## Modules

- Add dependencies with `go get <module>@<version>`, then `go mod tidy` after
  import changes — never hand-edit `go.sum`.
- Multi-module repos use `go.work`; run builds/tests from the right module
  directory.

## Traps that compile and still bite

- Writing to a nil map panics — `make(map[K]V)` first; reading is fine.
- Slices share their backing array: `append` may or may not allocate, so a
  callee appending to a passed slice can silently mutate the caller's data.
  `copy` to a fresh slice when independence matters.
- Zero values are meaningful — design types so the zero value is usable, and
  remember fields you don't set are not "unset", they're zero.
- `defer` runs at function end, not block end — a `defer f.Close()` inside a
  loop holds every file until the function returns.
- `range` over a map is deliberately randomized; never depend on its order.
- Loop-variable capture in goroutines/closures shares one variable before Go
  1.22 (check `go.mod`); pass it as an argument on older versions.

## Concurrency

- Every goroutine you start needs a guaranteed exit path — usually a
  `context.Context` it selects on. A goroutine with no exit is a leak.
- Sends on unbuffered channels block until received; close a channel only
  from the sender side, and never close it twice.
- Coordinate with `sync.WaitGroup` or `errgroup.Group`; protect shared state
  with a mutex or confine it to one goroutine and communicate instead.

## Interfaces

- Accept interfaces, return concrete types; keep interfaces small (one or two
  methods) and define them where they're *used*, not where they're
  implemented.
- A nil `*T` stored in an interface makes the interface non-nil — the classic
  bug: returning a typed nil error that fails `err != nil`. Return literal
  `nil` for the interface type.

## Idiomatic style

- Naming: `MixedCaps`, never underscores; short receiver names (`func (s
  *Server)`), consistent across a type's methods; no `Get` prefix on
  accessors (`user.Name()`, not `user.GetName()`); package names are short,
  lowercase, singular, and never `util`/`common`/`helpers`.
- Doc comments are full sentences starting with the name they document:
  `// Serve accepts incoming connections…`.
- Keep the happy path left-aligned: guard clauses and early returns instead
  of nested `if/else` ladders. Handle the error, return, and move on.
- Error values: sentinel errors are package-level `var ErrNotFound =
  errors.New("not found")`; error strings are lowercase without trailing
  punctuation (they get wrapped).
- `context.Context` is always the first parameter (`ctx`), never stored in a
  struct.
- Declare variables close to use with `:=`; avoid naked returns except in
  trivial functions; prefer composition (embedding) — Go has no inheritance
  to simulate.
- Don't introduce an interface before there are two implementations or a
  consumer that needs the seam; premature abstraction is un-Go.
- Let `gofmt` end all formatting debates.

## Tests

- Table-driven tests with `t.Run(name, …)` subtests are the house style; mark
  helpers with `t.Helper()` so failures point at the caller.
