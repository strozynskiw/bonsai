#!/usr/bin/env bash
set -euo pipefail

readonly ZERO_SHA="0000000000000000000000000000000000000000"

usage() {
  cat >&2 <<'EOF'
usage: check-migrations-immutable.sh --staged
       check-migrations-immutable.sh --range <base> <head>
       check-migrations-immutable.sh --history <base> <head>
EOF
  exit 2
}

require_commit() {
  local revision="$1"

  if ! git cat-file -e "${revision}^{commit}" 2>/dev/null; then
    echo "error: migration comparison commit is unavailable: ${revision}" >&2
    exit 2
  fi
}

report_changes() {
  local changed="$1"

  if [[ -z "$changed" ]]; then
    return
  fi

  echo "error: existing migration files were modified, deleted, renamed, or replaced:" >&2
  while IFS= read -r path; do
    printf '  %s\n' "$path" >&2
  done <<<"$changed"
  echo >&2
  echo "Migrations are immutable once committed. Create a new migration instead." >&2
  exit 1
}

check_staged() {
  local changed

  changed=$(git diff --cached --no-renames --diff-filter=MDT --name-only -- migrations/)
  report_changes "$changed"
}

check_range() {
  local base="$1"
  local head="$2"
  local changed

  if [[ "$base" == "$ZERO_SHA" ]]; then
    return
  fi

  require_commit "$base"
  require_commit "$head"
  changed=$(git diff --no-renames --diff-filter=MDT --name-only "$base" "$head" -- migrations/)
  report_changes "$changed"
}

check_history() {
  local base="$1"
  local head="$2"
  local tree_changes
  local history_changes
  local changed

  if [[ "$base" == "$ZERO_SHA" ]]; then
    return
  fi

  require_commit "$base"
  require_commit "$head"

  tree_changes=$(git diff --no-renames --diff-filter=MDT --name-only "$base" "$head" -- migrations/)
  history_changes=$(git log --no-renames --format= --diff-filter=MDT --name-only \
    "${base}..${head}" -- migrations/)
  changed=$(printf '%s\n%s\n' "$tree_changes" "$history_changes" | sed '/^$/d' | sort -u)
  report_changes "$changed"
}

case "${1:-}" in
  --staged)
    [[ "$#" -eq 1 ]] || usage
    check_staged
    ;;
  --range)
    [[ "$#" -eq 3 ]] || usage
    check_range "$2" "$3"
    ;;
  --history)
    [[ "$#" -eq 3 ]] || usage
    check_history "$2" "$3"
    ;;
  *)
    usage
    ;;
esac
