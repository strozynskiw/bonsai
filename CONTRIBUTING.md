# Contributing to bonsai

Thanks for your interest in bonsai! This is a small, fast-moving project — the
process is deliberately lightweight, but a few conventions keep it healthy.

## Before you start

- **Bugs and features** go through [GitHub issues](https://github.com/strozynskiw/bonsai/issues).
  Check for an existing issue first; the templates ask for what we actually need.
- **Direction** lives in the [milestones](https://github.com/strozynskiw/bonsai/milestones):
  `0.x` milestones lead to the 1.0 stability release, `1.x`/`2.0` are the future
  phases, and issues labeled `research` are experiments, not commitments. If you
  want to pick something up, a short comment on the issue avoids duplicate work.
  Issues labeled `good first issue` are self-contained starters.
- **Security vulnerabilities**: never open a public issue. Use
  [private vulnerability reporting](https://github.com/strozynskiw/bonsai/security/advisories/new)
  — see [SECURITY.md](SECURITY.md).

## Development setup

You need a recent stable Rust toolchain ([rustup](https://rustup.rs)).

```sh
git clone https://github.com/strozynskiw/bonsai.git
cd bonsai
cargo build
```

Run it against a scratch project (not your only copy of anything) while developing.

## Quality bar

Everything that lands on `master` must pass:

```sh
cargo fmt --all --check
cargo clippy -- -D warnings
cargo test
```

Notes that save you time:

- The crate is **binary-only** (no lib target). Run a focused test with a single
  filter: `cargo test <filter>` — and confirm you see `test result: ok`, since a
  piped tail can mask failures.
- Every behavior change needs test coverage next to the code it changes. TUI
  changes get reducer/render coverage (see the existing patterns in
  `src/tui/.../tests`).
- Comments explain *why* something is the way it is — especially anything that
  looks simplifiable but isn't (those carry "deliberate, don't simplify" notes;
  please keep them).

## Pull requests

- Keep PRs focused: one logical change per PR.
- Use conventional-commit style titles, matching the history:
  `feat(tui): …`, `fix(storage): …`, `chore(release): …`, `refactor(tool): …`.
- PRs are **squash-merged** — the PR title and body become the commit, so write
  them like the commit message you want in history.
- Fill in the PR template, including how the change was verified.
- CI must be green; a maintainer review is required before merge.

## Scope guidance

bonsai has an explicit product contract (terminal-first, provider-independent,
local-first, safety-conscious). Large features — new surfaces, protocols,
providers — should start as an issue conversation before code, so the design
lands once. Small fixes and polish can go straight to a PR.

## License

By contributing, you agree that your contributions are licensed under the
project's [MIT License](LICENSE).
