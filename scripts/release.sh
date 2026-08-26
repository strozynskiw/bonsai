#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  scripts/release.sh <version> [--no-push] [--skip-checks] [--tag]

Examples:
  scripts/release.sh 0.2.0-rc.1
  scripts/release.sh v0.2.0-rc.1
  scripts/release.sh 0.2.0-rc.1 --no-push
  scripts/release.sh 0.2.0-rc.1 --tag --no-push

This script:
  1. updates Cargo.toml, Cargo.lock, and public install references
  2. runs fmt, clippy, tests, and release build checks
  3. commits the bump as chore(release): prepare <tag>
  4. creates an annotated <tag>
  5. pushes the commit and tag to GitHub unless --no-push is set

--tag only creates the tag: it skips the bump, the checks, and the release
commit, and tags HEAD. It still requires Cargo.toml and the public install
references to already be at <version>, so the tag cannot land on a tree that
disagrees with it. Use this when the bump was committed separately and only
the tag is missing.

Pushing the tag starts .github/workflows/release.yml, which builds Linux
(x86_64, aarch64) and macOS (x86_64, arm64) binaries, publishes a GitHub
prerelease with signed manifests, and opens or updates the Homebrew tap PR
for release/<tag>. Linux and macOS users install via:
  curl -fsSL https://raw.githubusercontent.com/strozynskiw/bonsai/master/install.sh | sh
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
tag_only=0

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
    --tag)
      tag_only=1
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

if [ "$tag_only" -eq 1 ]; then
  # Tagging a tree that disagrees with the tag would ship a release whose
  # binaries report a different version than the tag promises, so the same
  # references the bump would have rewritten are verified instead.
  [ "$old_version" = "$version" ] ||
    die "--tag requires Cargo.toml to already be at $version (it is at $old_version)"
  for public_file in site/index.html install.sh; do
    grep -q -- "$tag" "$public_file" ||
      die "--tag requires $public_file to reference $tag"
  done
else

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
for filename in ("site/index.html", "install.sh"):
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

fi

# --tag only labels a commit that is already in history, so it rebuilds
# nothing: the checks belong to the commit that introduced the bump, not to
# the act of naming it.
if [ "$run_checks" -eq 1 ] && [ "$tag_only" -eq 0 ]; then
  cargo fmt --all --check
  cargo clippy --locked --all-targets --all-features -- -D warnings
  cargo test --locked
  RUSTFLAGS="-D warnings" cargo build --release --locked
fi

if [ "$tag_only" -eq 0 ]; then
  git diff -- Cargo.toml Cargo.lock site/index.html install.sh
  git add Cargo.toml Cargo.lock site/index.html install.sh
  git commit -m "chore(release): prepare $tag"
fi
git tag -a "$tag" -m "Release $tag"

if [ "$push" -eq 1 ]; then
  git push "$remote" "$current_branch"
  git push "$remote" "$tag"
  printf 'release: pushed %s and %s to %s\n' "$current_branch" "$tag" "$remote"
  printf 'release: GitHub Actions will build and publish the release from %s\n' "$tag"
  printf 'release: after it succeeds, the Homebrew tap PR will be opened or updated\n'
  printf 'release: Linux and macOS users can then install via the install.sh script\n'
else
  printf 'release: prepared %s locally; push with:\n' "$tag"
  printf '  git push %s %s\n' "$remote" "$current_branch"
  printf '  git push %s %s\n' "$remote" "$tag"
fi
