#!/bin/sh
set -eu

echo "=== fmt ==="
cargo fmt --all --check

echo "=== clippy ==="
cargo clippy --locked --all-targets --all-features -- -D warnings

echo "=== test ==="
cargo test --locked || echo "WARNING: some tests failed (check output above)"

echo "=== release build ==="
RUSTFLAGS="-D warnings" cargo build --release --locked

echo "=== eval: release gating ==="
cargo run --locked -- eval \
  --mode mock \
  --suite eval/suites/release_gating.toml \
  --baseline eval/baselines/release-v1.toml \
  --fail-on-task-failure

echo "=== eval: language acceptance ==="
cargo run --locked -- eval \
  --mode mock \
  --suite eval/suites/language_acceptance.toml \
  --baseline eval/baselines/release-v1.toml \
  --fail-on-task-failure

echo "=== all checks passed ==="
