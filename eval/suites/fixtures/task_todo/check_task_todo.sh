#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

pass() {
  printf 'ok - %s\n' "$*"
}

capture() {
  local out
  set +e
  out="$("$@" 2>&1)"
  local status=$?
  set -e
  printf '%s' "$out"
  return "$status"
}

lowercase() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  local label="$3"
  [[ "$(lowercase "$haystack")" == *"$(lowercase "$needle")"* ]] ||
    fail "$label: expected output to contain '$needle'. Output was: $haystack"
}

assert_not_contains() {
  local haystack="$1"
  local needle="$2"
  local label="$3"
  [[ "$haystack" != *"$needle"* ]] ||
    fail "$label: expected output not to contain '$needle'. Output was: $haystack"
}

assert_fails_with() {
  local label="$1"
  local needle="$2"
  shift 2
  local out
  if out="$(capture "$@")"; then
    fail "$label: command succeeded unexpectedly. Output was: $out"
  fi
  assert_contains "$out" "$needle" "$label"
  assert_contains "$out" "line" "$label"
  pass "$label"
}

[[ -f Cargo.toml ]] || fail "run from the generated tasklog Rust project root"
command -v cargo >/dev/null 2>&1 || fail "cargo is required"
command -v python3 >/dev/null 2>&1 || fail "python3 is required"

if grep -E '^[[:space:]]*(axum|tokio|sqlx)[[:space:]]*=' Cargo.toml >/dev/null; then
  fail "Cargo.toml includes a web/db/runtime dependency disallowed by the prompt"
fi

tmp_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

export TASKLOG_TMP="$tmp_dir"
python3 <<'PY'
import datetime as dt
import os
import pathlib

root = pathlib.Path(os.environ["TASKLOG_TMP"])
today = dt.date.today()
soon = today + dt.timedelta(days=3)
future = today + dt.timedelta(days=21)
overdue = today - dt.timedelta(days=1)
done = today - dt.timedelta(days=2)

(root / "valid.tasklog").write_text(f"""# Project: Website Refresh
@owner alice
@timezone UTC

// comment lines should be ignored

[Backlog]
- [ ] T-1 Write launch copy @high due:{soon.isoformat()} #marketing #copy
- [ ] T-2 Pick hero images @medium due:{future.isoformat()} #design

[In Progress]
- [ ] T-3 Implement pricing page @high due:{overdue.isoformat()} #frontend
- [ ] T-5 Add analytics events @low due:{future.isoformat()} #frontend #analytics

[Done]
- [x] T-4 Draft sitemap @low done:{done.isoformat()} #planning
""")

(root / "duplicate.tasklog").write_text(f"""# Project: Duplicate IDs
[Backlog]
- [ ] T-1 First task @low due:{future.isoformat()}
- [ ] T-1 Duplicate task @medium due:{future.isoformat()}
""")
(root / "bad_date.tasklog").write_text("""# Project: Bad Date
[Backlog]
- [ ] T-9 Broken due date @low due:not-a-date
""")
(root / "bad_priority.tasklog").write_text(f"""# Project: Bad Priority
[Backlog]
- [ ] T-10 Wrong priority @urgent due:{future.isoformat()}
""")
(root / "outside_section.tasklog").write_text(f"""# Project: Outside Section
- [ ] T-11 Missing section @low due:{future.isoformat()}
""")
(root / "bad_section.tasklog").write_text("""# Project: Bad Section
[Missing close
- [ ] T-12 Malformed section @low
""")
PY

cargo fmt --all -- --check
pass "cargo fmt --check"
cargo clippy --all-targets --all-features -- -D warnings
pass "cargo clippy"
cargo test --locked
pass "cargo test --locked"
cargo build --quiet --bin tasklog
bin="./target/debug/tasklog"
[[ -x "$bin" ]] || fail "tasklog binary is not executable"

valid="$tmp_dir/valid.tasklog"
out="$(capture "$bin" validate "$valid")" || fail "valid file failed: $out"
assert_contains "$out" "ok" "validate valid file"

summary="$(capture "$bin" summary "$valid")" || fail "summary failed: $summary"
for expected in "Website Refresh" "Backlog" "In Progress" "Done" high medium low frontend marketing; do
  assert_contains "$summary" "$expected" "summary output"
done
pass "summary includes project, sections, priorities, and tags"

list_all="$(capture "$bin" list "$valid")" || fail "list failed: $list_all"
for expected in T-1 T-2 T-3 T-4 T-5 "Write launch copy"; do
  assert_contains "$list_all" "$expected" "list output"
done
pass "list shows all tasks"

list_overdue="$(capture "$bin" list "$valid" --status overdue)" || fail "overdue filter failed"
assert_contains "$list_overdue" T-3 "overdue filter"
assert_not_contains "$list_overdue" T-1 "overdue filter"
assert_not_contains "$list_overdue" T-4 "overdue filter"

list_due_soon="$(capture "$bin" list "$valid" --status due-soon)" || fail "due-soon filter failed"
assert_contains "$list_due_soon" T-1 "due-soon filter"
assert_not_contains "$list_due_soon" T-3 "due-soon filter"
assert_not_contains "$list_due_soon" T-4 "due-soon filter"

list_done="$(capture "$bin" list "$valid" --status done)" || fail "done filter failed"
assert_contains "$list_done" T-4 "done filter"
assert_not_contains "$list_done" T-1 "done filter"

list_tag="$(capture "$bin" list "$valid" --tag frontend)" || fail "tag filter failed"
assert_contains "$list_tag" T-3 "tag filter"
assert_contains "$list_tag" T-5 "tag filter"
assert_not_contains "$list_tag" T-1 "tag filter"

json_one="$tmp_dir/export-one.json"
json_two="$tmp_dir/export-two.json"
"$bin" export "$valid" --format json >"$json_one"
"$bin" export "$valid" --format json >"$json_two"
cmp -s "$json_one" "$json_two" || fail "JSON export is not deterministic"

python3 - "$json_one" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text())

def values(node):
    if isinstance(node, dict):
        for key, value in node.items():
            yield str(key)
            yield from values(value)
    elif isinstance(node, list):
        for item in node:
            yield from values(item)
    elif node is not None:
        yield str(node)

joined = "\n".join(values(data))
required = [
    "Website Refresh", "alice", "Backlog", "In Progress", "Done",
    "T-1", "T-2", "T-3", "T-4", "T-5", "frontend", "marketing",
]
missing = [item for item in required if item not in joined]
if missing:
    raise SystemExit(f"export JSON missing expected values: {missing}")
PY
pass "deterministic JSON export contains project data"

assert_fails_with "duplicate id validation" duplicate "$bin" validate "$tmp_dir/duplicate.tasklog"
assert_fails_with "bad date validation" date "$bin" validate "$tmp_dir/bad_date.tasklog"
assert_fails_with "bad priority validation" priority "$bin" validate "$tmp_dir/bad_priority.tasklog"
assert_fails_with "task outside section validation" section "$bin" validate "$tmp_dir/outside_section.tasklog"
assert_fails_with "bad section validation" section "$bin" validate "$tmp_dir/bad_section.tasklog"

[[ -f README.md ]] || fail "README.md is required"
readme="$(cat README.md)"
for expected in tasklog validate summary list export; do
  assert_contains "$readme" "$expected" README
done

printf '\nAll tasklog completion checks passed.\n'
