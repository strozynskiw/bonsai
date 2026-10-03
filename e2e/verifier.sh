#!/usr/bin/env bash
# Launch the real Bonsai TUI behind one fail-closed, evidence-producing state
# boundary. Both agents and the tmux e2e driver use this entry point.
set -euo pipefail

VERIFIER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$VERIFIER_DIR/.." && pwd)"
DEFAULT_RUNS_ROOT="$REPO_ROOT/target/tui-verification/runs"
MAX_RETAINED_RUNS=20

usage() {
  cat <<'EOF'
Usage: e2e/verifier.sh [options] [-- bonsai-args...]

Options:
  --state-root PATH          Use a trusted harness state root (e2e only)
  --evidence-dir PATH        Write the evidence manifest under PATH
  --allow-reusable-roots     Opt into caller-owned roots (trusted harness only)
  --binary PATH              Bonsai binary to launch
  --provider-base-url URL    Loopback/dead OpenAI-compatible test endpoint
  --enable-opencode-test-key Seed a fixed synthetic OpenCode key
  --allow-shared-state       Explicitly permit a parent-state path (destructive)
  -h, --help                 Show this help

With no state options, the verifier allocates a unique retained run under
target/tui-verification/runs. It never inherits provider credentials or config
homes from the parent process.
EOF
}

die() {
  echo "error: $*" >&2
  exit 2
}

same_or_nested_path() {
  local candidate="$1" parent="$2"
  [[ "$candidate" == "$parent" || "$candidate" == "$parent/"* ]]
}

canonical_prospective_dir() {
  local path="$1" parent leaf canonical_parent
  if [[ "$path" != /* ]]; then
    path="$PWD/$path"
  fi
  if [[ -d "$path" ]]; then
    (cd "$path" && pwd -P)
    return
  fi
  parent="$(dirname "$path")"
  leaf="$(basename "$path")"
  canonical_parent="$(canonical_prospective_dir "$parent")"
  case "$leaf" in
    .) printf '%s\n' "$canonical_parent" ;;
    ..) dirname "$canonical_parent" ;;
    *) printf '%s/%s\n' "$canonical_parent" "$leaf" ;;
  esac
}

# Reusable roots may contain aliases into user state. Reject links anywhere in
# their existing tree, including evidence files, before mkdir/truncation.
require_unlinked_tree() {
  local path="$1" links
  [[ ! -L "$path" ]] || die "refusing symlinked verifier path: $path"
  if [[ -d "$path" ]]; then
    links="$(find "$path" -type l -print -quit)"
    [[ -z "$links" ]] || die "refusing symlink inside verifier state: $links"
    links="$(find "$path" -type f -links +1 -print -quit)"
    [[ -z "$links" ]] || die "refusing hard link inside verifier state: $links"
  fi
}

# Portable across macOS and Linux. Never steal a stale lock: an interrupted
# harness must explicitly clean it up before reusing the root.
acquire_root_lock() {
  local root="$1" lock="${1}${2:-.verifier-lock}" attempts=0
  while ! mkdir "$lock" 2>/dev/null; do
    [[ -d "$lock" && ! -L "$lock" ]] || die "cannot lock verifier root: $root"
    attempts=$((attempts + 1))
    (( attempts < 300 )) || die "verifier root is busy: $root"
    sleep 0.1
  done
  root_locks+=("$lock")
}

release_root_locks() {
  local index
  # Release retention protection last, after nested root locks are removed.
  for (( index=${#root_locks[@]}-1; index>=0; index-- )); do
    rmdir "${root_locks[index]}"
  done
}

require_loopback_provider() {
  case "$1" in
    http://127.0.0.1:*|https://127.0.0.1:*|http://localhost:*|https://localhost:*) ;;
    *) die "--provider-base-url must use a loopback test endpoint" ;;
  esac
}

sha256_file() {
  local path="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | cut -d ' ' -f 1
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | cut -d ' ' -f 1
  else
    printf 'unavailable'
  fi
}

prune_retained_runs() {
  local runs_root="$1" retained=0 run lock
  while IFS= read -r run; do
    [[ "$run" != *.verifier-retention-lock ]] || continue
    lock="${run}.verifier-retention-lock"
    # Reuse holds the same lifecycle lock through evidence finalization.
    mkdir "$lock" 2>/dev/null || continue
    if [[ ! -e "$run/.active" && -f "$run/.completed" ]]; then
      retained=$((retained + 1))
      if (( retained > MAX_RETAINED_RUNS )); then
        rm -rf "$run"
      fi
    fi
    rmdir "$lock"
  done < <(find "$runs_root" -mindepth 1 -maxdepth 1 -type d -print | sort -r)
}

state_root=""
evidence_dir=""
binary="${BONSAI_VERIFIER_BIN:-$REPO_ROOT/target/debug/bonsai}"
provider_base_url="${BONSAI_VERIFIER_PROVIDER_BASE_URL:-http://127.0.0.1:9/v1}"
opencode_api_key=""
allow_shared_state=false
allow_reusable_roots=false
root_locks=()
bonsai_args=()

while (( $# > 0 )); do
  case "$1" in
    --state-root)
      (( $# >= 2 )) || die "--state-root requires a path"
      state_root="$2"
      shift 2
      ;;
    --evidence-dir)
      (( $# >= 2 )) || die "--evidence-dir requires a path"
      evidence_dir="$2"
      shift 2
      ;;
    --binary)
      (( $# >= 2 )) || die "--binary requires a path"
      binary="$2"
      shift 2
      ;;
    --provider-base-url)
      (( $# >= 2 )) || die "--provider-base-url requires a URL"
      provider_base_url="$2"
      shift 2
      ;;
    --enable-opencode-test-key)
      opencode_api_key="e2e-opencode-test"
      shift
      ;;
    --allow-shared-state)
      allow_shared_state=true
      shift
      ;;
    --allow-reusable-roots)
      allow_reusable_roots=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      bonsai_args=("$@")
      break
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

# This gate precedes allocation, mkdir, lock creation, and evidence truncation.
if [[ -n "$state_root" || -n "$evidence_dir" ]]; then
  [[ "$allow_reusable_roots" == true ]] \
    || die "caller-supplied roots require --allow-reusable-roots (trusted harness only)"
fi
umask 077
trap release_root_locks EXIT
[[ -x "$binary" ]] || die "Bonsai binary is not executable: $binary (run cargo build first)"
require_loopback_provider "$provider_base_url"

owned_run=""
if [[ -z "$state_root" ]]; then
  runs_root="${BONSAI_VERIFIER_RUNS_ROOT:-$DEFAULT_RUNS_ROOT}"
  mkdir -p "$runs_root"
  owned_run="$(mktemp -d "$runs_root/$(date -u +%Y%m%dT%H%M%SZ)-$$.XXXXXX")"
  state_root="$owned_run/state"
  : "${evidence_dir:=$owned_run/evidence}"
  touch "$owned_run/.active"
else
  : "${evidence_dir:=$state_root/evidence}"
fi

parent_bonsai_home="${BONSAI_HOME:-}"
if [[ -z "$parent_bonsai_home" && -n "${HOME:-}" ]]; then
  parent_bonsai_home="$HOME/.bonsai"
fi
if [[ -n "$parent_bonsai_home" ]]; then
  parent_bonsai_home="$(canonical_prospective_dir "$parent_bonsai_home")"
fi
state_root="$(canonical_prospective_dir "$state_root")"
evidence_dir="$(canonical_prospective_dir "$evidence_dir")"
shared_state=false
if [[ -n "$parent_bonsai_home" ]] \
  && { same_or_nested_path "$state_root" "$parent_bonsai_home" \
    || same_or_nested_path "$parent_bonsai_home" "$state_root"; }; then
  if [[ "$allow_shared_state" != true ]]; then
    die "refusing verifier state overlapping parent BONSAI_HOME ($parent_bonsai_home); omit --state-root or pass --allow-shared-state explicitly"
  fi
  shared_state=true
fi
if [[ "$allow_shared_state" != true ]]; then
  require_unlinked_tree "$state_root"
  require_unlinked_tree "$evidence_dir"
  state_root="$(canonical_prospective_dir "$state_root")"
  evidence_dir="$(canonical_prospective_dir "$evidence_dir")"
  if [[ -n "$parent_bonsai_home" ]] \
    && { same_or_nested_path "$evidence_dir" "$parent_bonsai_home" \
      || same_or_nested_path "$parent_bonsai_home" "$evidence_dir"; }; then
    die "refusing verifier evidence overlapping parent BONSAI_HOME ($parent_bonsai_home)"
  fi
fi
if [[ "$allow_reusable_roots" == true ]]; then
  # Protect marked retained ancestors before reopening their state/evidence.
  # Retention must claim the same sibling lock before checking or deleting.
  retained_ancestors=()
  for root in "$state_root" "$evidence_dir"; do
    ancestor="$root"
    while [[ "$ancestor" != / ]]; do
      if [[ -f "$ancestor/.completed" || -e "$ancestor/.active" ]]; then
        retained_ancestors+=("$ancestor")
      fi
      ancestor="$(dirname "$ancestor")"
    done
  done
  if (( ${#retained_ancestors[@]} > 0 )); then
    while IFS= read -r ancestor; do
      acquire_root_lock "$ancestor" .verifier-retention-lock
    done < <(printf '%s\n' "${retained_ancestors[@]}" | sort -u)
  fi
  # Sorted acquisition prevents deadlocks when callers share both roots.
  while IFS= read -r root; do
    mkdir -p "$(dirname "$root")"
    acquire_root_lock "$root"
  done < <(printf '%s\n' "$state_root" "$evidence_dir" | sort -u)
  if [[ "$allow_shared_state" != true ]]; then
    require_unlinked_tree "$state_root"
    require_unlinked_tree "$evidence_dir"
  fi
fi
mkdir -p "$state_root" "$evidence_dir"
state_root="$(cd "$state_root" && pwd -P)"
evidence_dir="$(cd "$evidence_dir" && pwd -P)"

isolated_home="$state_root/home"
if [[ "$shared_state" == true ]]; then
  isolated_bonsai_home="$state_root"
else
  isolated_bonsai_home="$state_root/bonsai"
fi
isolated_codex_home="$state_root/codex"
isolated_tmp="$state_root/tmp"
mkdir -p \
  "$isolated_home" \
  "$isolated_bonsai_home" \
  "$isolated_codex_home" \
  "$isolated_tmp" \
  "$state_root/xdg/config" \
  "$state_root/xdg/data" \
  "$state_root/xdg/cache" \
  "$state_root/xdg/state" \
  "$evidence_dir/screens"
: > "$evidence_dir/inputs.log"

manifest="$evidence_dir/manifest.txt"
started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [[ "$binary" = /* ]]; then
  binary_path="$binary"
else
  binary_path="$(cd "$(dirname "$binary")" && pwd -P)/$(basename "$binary")"
fi
binary_identity="$(sha256_file "$binary_path")"
worktree_head="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || printf 'unavailable')"
if git -C "$REPO_ROOT" diff --quiet --ignore-submodules HEAD -- 2>/dev/null; then
  worktree_state="clean"
else
  worktree_state="dirty"
fi
{
  printf 'schema_version=1\n'
  printf 'started_at=%s\n' "$started_at"
  printf 'worktree_root=%s\n' "$REPO_ROOT"
  printf 'worktree_head=%s\n' "$worktree_head"
  printf 'worktree_state=%s\n' "$worktree_state"
  printf 'binary=%s\n' "$binary_path"
  printf 'binary_sha256=%s\n' "$binary_identity"
  printf 'state_root=%s\n' "$state_root"
  printf 'bonsai_home=%s\n' "$isolated_bonsai_home"
  printf 'evidence_dir=%s\n' "$evidence_dir"
  printf 'reusable_roots=%s\n' "$allow_reusable_roots"
  printf 'shared_state=%s\n' "$shared_state"
  printf 'provider_base_url=%s\n' "$provider_base_url"
  printf 'argv='
  printf '%q ' "$binary_path"
  if (( ${#bonsai_args[@]} > 0 )); then
    printf '%q ' "${bonsai_args[@]}"
  fi
  printf '\n'
} > "$manifest"

echo "Bonsai TUI verifier evidence: $evidence_dir" >&2

finalized=false
finalize() {
  local exit_code="$1" finished_at
  [[ "$finalized" == false ]] || return
  finalized=true
  finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  {
    printf 'finished_at=%s\n' "$finished_at"
    printf 'exit_code=%s\n' "$exit_code"
    if [[ -f "$isolated_bonsai_home/bonsai.db" ]]; then
      printf 'database=%s\n' "$isolated_bonsai_home/bonsai.db"
      printf 'database_sha256=%s\n' "$(sha256_file "$isolated_bonsai_home/bonsai.db")"
    else
      printf 'database=absent\n'
    fi
  } >> "$manifest"
  if [[ -n "$owned_run" ]]; then
    touch "$owned_run/.completed"
    rm -f "$owned_run/.active"
    prune_retained_runs "$(dirname "$owned_run")"
  fi
}
trap 'exit 129' HUP
trap 'exit 143' TERM
trap 'exit_code=$?; finalize "$exit_code"; release_root_locks' EXIT

isolated_env=(
  env -i
  PATH="${PATH:-/usr/bin:/bin}"
  TERM="${TERM:-xterm-256color}"
  LANG="${LANG:-C.UTF-8}"
  TZ=UTC
  HOME="$isolated_home"
  BONSAI_HOME="$isolated_bonsai_home"
  CODEX_HOME="$isolated_codex_home"
  XDG_CONFIG_HOME="$state_root/xdg/config"
  XDG_DATA_HOME="$state_root/xdg/data"
  XDG_CACHE_HOME="$state_root/xdg/cache"
  XDG_STATE_HOME="$state_root/xdg/state"
  TMPDIR="$isolated_tmp"
  TMP="$isolated_tmp"
  TEMP="$isolated_tmp"
  BONSAI_DOTENV=0
  BONSAI_DISABLE_KEYRING=1
  BONSAI_DISABLE_MODELS_FETCH=1
  BONSAI_MEMORY_EMBEDDINGS=off
  OPENAI_COMPATIBLE_BASE_URL="$provider_base_url"
  OPENAI_COMPATIBLE_MODEL=mock-model
  OPENAI_COMPATIBLE_API_KEY=e2e-test
  OPENCODE_API_KEY="$opencode_api_key"
  "$binary_path"
)
if (( ${#bonsai_args[@]} > 0 )); then
  isolated_env+=("${bonsai_args[@]}")
fi

set +e
"${isolated_env[@]}"
exit_code=$?
set -e
exit "$exit_code"
