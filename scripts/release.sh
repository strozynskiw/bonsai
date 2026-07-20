#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  scripts/release.sh <version> [--no-push] [--skip-checks]

Examples:
  scripts/release.sh 0.2.0-rc.1
  scripts/release.sh v0.2.0-rc.1
  scripts/release.sh 0.2.0-rc.1 --no-push

This script:
  1. updates Cargo.toml, Cargo.lock, and public install references
  2. runs fmt, clippy, tests, and release build checks
  3. commits the bump as chore(release): prepare <tag>
  4. creates an annotated <tag>
  5. pushes the commit and tag to GitHub unless --no-push is set

Pushing the tag starts .github/workflows/release.yml, which publishes the
GitHub release and release assets.
EOF
}

die() {
  printf 'release: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

version_arg=""
push=1
run_checks=1

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --no-push)
      push=0
      ;;
    --skip-checks)
      run_checks=0
      ;;
    -*)
      usage
      die "unknown option: $1"
      ;;
    *)
      if [ -n "$version_arg" ]; then
        usage
        die "version was provided more than once"
      fi
      version_arg="$1"
      ;;
  esac
  shift
done

[ -n "$version_arg" ] || {
  usage
  die "missing version"
}

need cargo
need git
need python3

case "$version_arg" in
  v*) version="${version_arg#v}" ;;
  *) version="$version_arg" ;;
esac
tag="v$version"

if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  die "version must be semver, for example 0.2.0-rc.1"
fi

if ! git rev-parse --show-toplevel >/dev/null 2>&1; then
  die "not inside a git repository"
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

current_branch="$(git branch --show-current)"
[ -n "$current_branch" ] || die "detached HEAD; check out a branch before releasing"

if ! git diff --quiet || ! git diff --cached --quiet; then
  die "working tree has uncommitted changes"
fi

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  die "local tag already exists: $tag"
fi

remote="${BONSAI_RELEASE_REMOTE:-origin}"
if [ "$push" -eq 1 ]; then
  git remote get-url "$remote" >/dev/null
  remote_tag="$(git ls-remote --tags "$remote" "refs/tags/$tag")"
  if [ -n "$remote_tag" ]; then
    die "remote tag already exists on $remote: $tag"
  fi
fi

old_version="$(python3 - <<'PY'
import pathlib
import re

manifest = pathlib.Path("Cargo.toml").read_text()
match = re.search(r'(?m)^version = "([^"]+)"$', manifest)
if not match:
    raise SystemExit("could not find package version in Cargo.toml")
print(match.group(1))
PY
)"
old_tag="v$old_version"

[ "$old_version" != "$version" ] || die "Cargo.toml is already at $version"

python3 - "$old_version" "$version" <<'PY'
import pathlib
import re
import sys

old_version, new_version = sys.argv[1], sys.argv[2]
old_tag = f"v{old_version}"
new_tag = f"v{new_version}"

cargo_toml = pathlib.Path("Cargo.toml")
manifest = cargo_toml.read_text()
updated_manifest, count = re.subn(
    r'(?m)^version = "[^"]+"$',
    f'version = "{new_version}"',
    manifest,
    count=1,
)
if count != 1:
    raise SystemExit("could not update Cargo.toml package version")
public_updates = []
for filename in ("README.md", "site/index.html", "install.sh"):
    path = pathlib.Path(filename)
    original = path.read_text()
    updated = original.replace(old_tag, new_tag)
    if updated == original:
        raise SystemExit(f"could not find {old_tag} in {filename}")
    public_updates.append((path, updated))

cargo_toml.write_text(updated_manifest)
for path, updated in public_updates:
    path.write_text(updated)
PY

cargo update -p bonsai --offline

if [ "$run_checks" -eq 1 ]; then
  cargo fmt --all --check
  cargo clippy --locked --all-targets --all-features -- -D warnings
  cargo test --locked
  RUSTFLAGS=-D warnings cargo build --release --locked
fi

git diff -- Cargo.toml Cargo.lock README.md site/index.html install.sh
git add Cargo.toml Cargo.lock README.md site/index.html install.sh
git commit -m "chore(release): prepare $tag"
git tag -a "$tag" -m "Release $tag"

if [ "$push" -eq 1 ]; then
  git push "$remote" "$current_branch"
  git push "$remote" "$tag"
  printf 'release: pushed %s and %s to %s\n' "$current_branch" "$tag" "$remote"
  printf 'release: GitHub Actions will publish the release from %s\n' "$tag"
else
  printf 'release: prepared %s locally; push with:\n' "$tag"
  printf '  git push %s %s\n' "$remote" "$current_branch"
  printf '  git push %s %s\n' "$remote" "$tag"
fi
