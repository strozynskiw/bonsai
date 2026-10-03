#!/usr/bin/env bash
set -euo pipefail

readonly CHECK_SCRIPT="$(cd "$(dirname "$0")" && pwd)/check-migrations-immutable.sh"
TEST_REPO=""

cleanup() {
  if [[ -n "$TEST_REPO" ]]; then
    rm -rf "$TEST_REPO"
  fi
}
trap cleanup EXIT

assert_passes() {
  local description="$1"
  shift

  if ! "$@" >/dev/null 2>&1; then
    echo "FAIL: expected success: ${description}" >&2
    exit 1
  fi
}

assert_guard_rejects() {
  local description="$1"
  local status
  shift

  set +e
  "$@" >/dev/null 2>&1
  status=$?
  set -e
  if [[ "$status" -ne 1 ]]; then
    echo "FAIL: expected failure: ${description}" >&2
    echo "guard exited with ${status}, expected 1" >&2
    exit 1
  fi
}

commit_all() {
  local message="$1"

  git add --all
  git commit --quiet -m "$message"
}

TEST_REPO=$(mktemp -d)
cd "$TEST_REPO"
git init --quiet
git config user.name "Migration Guard Test"
git config user.email "migration-guard@example.invalid"

mkdir migrations
printf '%s\n' 'CREATE TABLE initial (id INTEGER);' > migrations/0001_initial.sql
commit_all "initial migration"
INITIAL=$(git rev-parse HEAD)

assert_passes "an unchanged range" bash "$CHECK_SCRIPT" --range "$INITIAL" HEAD

printf '%s\n' 'CREATE TABLE changed (id INTEGER);' > migrations/0001_initial.sql
git add migrations/0001_initial.sql
assert_guard_rejects "a staged modification" bash "$CHECK_SCRIPT" --staged
git reset --hard --quiet HEAD

printf '%s\n' 'CREATE TABLE second (id INTEGER);' > migrations/0002_second.sql
git add migrations/0002_second.sql
assert_passes "a staged new migration" bash "$CHECK_SCRIPT" --staged
commit_all "add second migration"
ADDED=$(git rev-parse HEAD)
assert_passes "a range containing only a new migration" \
  bash "$CHECK_SCRIPT" --range "$INITIAL" "$ADDED"
assert_passes "history containing only a new migration" \
  bash "$CHECK_SCRIPT" --history "$INITIAL" "$ADDED"

printf '%s\n' 'CREATE TABLE second_changed (id INTEGER);' > migrations/0002_second.sql
commit_all "modify second migration"
MODIFIED=$(git rev-parse HEAD)
assert_guard_rejects "a modified migration in a direct range" \
  bash "$CHECK_SCRIPT" --range "$ADDED" "$MODIFIED"
assert_guard_rejects "a migration added and then modified in release history" \
  bash "$CHECK_SCRIPT" --history "$INITIAL" "$MODIFIED"

git reset --hard --quiet "$ADDED"
git switch --quiet -c merge-side-one
printf '%s\n' 'side one' > side-one.txt
commit_all "add first merge side"

git switch --quiet -c merge-side-two "$ADDED"
printf '%s\n' 'side two' > side-two.txt
commit_all "add second merge side"

git switch --quiet -c merge-main "$ADDED"
git merge --quiet --no-ff --no-commit merge-side-one
printf '%s\n' 'CREATE TABLE merge_changed (id INTEGER);' > migrations/0002_second.sql
commit_all "modify migration in merge"
git merge --quiet --no-ff --no-commit merge-side-two
printf '%s\n' 'CREATE TABLE second (id INTEGER);' > migrations/0002_second.sql
commit_all "restore migration in merge"
MERGE_RESTORED=$(git rev-parse HEAD)

assert_passes "merge-only mutation restored in the endpoint tree" \
  bash "$CHECK_SCRIPT" --range "$ADDED" "$MERGE_RESTORED"
assert_guard_rejects "merge-only mutation restored later in merge history" \
  bash "$CHECK_SCRIPT" --history "$ADDED" "$MERGE_RESTORED"

git reset --hard --quiet "$ADDED"
git mv migrations/0001_initial.sql migrations/0001_renamed.sql
assert_guard_rejects "a staged rename" bash "$CHECK_SCRIPT" --staged

echo "migration immutability checks passed"
