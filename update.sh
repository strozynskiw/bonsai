#!/bin/sh
# Update an existing Bonsai install to the newest published release.
#
#   curl -fsSL https://raw.githubusercontent.com/strozynskiw/bonsai/master/update.sh | sh
#
# Environment:
#   BONSAI_INSTALL_DIR  Where bonsai lives. Default: directory of `bonsai` on
#                       PATH, else $HOME/.local/bin
#   BONSAI_VERSION      Install this tag instead of the newest release. Also the
#                       way to downgrade, since --force is required to move off
#                       an already-current install.
#   --force             Reinstall even when already on the target version.
#
# This is a thin front-end: once it decides an update is warranted it delegates
# the download, signature check, and install to install.sh, so the verification
# chain lives in exactly one place. It prefers the install.sh sitting next to it
# (git checkout) and otherwise fetches the published one.

set -eu

REPO="strozynskiw/bonsai"
RAW_INSTALLER="https://raw.githubusercontent.com/$REPO/master/install.sh"

FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) printf 'bonsai: error: unknown argument: %s\n' "$arg" >&2; exit 1 ;;
  esac
done

log() { printf 'bonsai: %s\n' "$1" >&2; }
die() { printf 'bonsai: error: %s\n' "$1" >&2; exit 1; }

# ------------------------------------------------------- locate the install ---

if [ -n "${BONSAI_INSTALL_DIR:-}" ]; then
  install_dir="$BONSAI_INSTALL_DIR"
elif existing="$(command -v bonsai 2>/dev/null)"; then
  install_dir="$(dirname "$existing")"
else
  install_dir="$HOME/.local/bin"
fi
binary="$install_dir/bonsai"

# `bonsai --version` prints "bonsai <semver>"; the release tags are "v<semver>".
current=""
if [ -x "$binary" ]; then
  current="v$("$binary" --version 2>/dev/null | awk 'NR==1 { print $NF }')"
  [ "$current" = "v" ] && current=""
fi

if [ -z "$current" ]; then
  log "no existing install found in $install_dir — installing fresh"
else
  log "installed: $current"
fi

# ------------------------------------------------------ resolve the target ---

fetch_stdout() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$1"
  else
    die "need curl or wget"
  fi
}

if [ -n "${BONSAI_VERSION:-}" ]; then
  target="$BONSAI_VERSION"
else
  # Releases are published as prereleases, which /releases/latest omits.
  # sed -E is deliberate: BRE alternation (\|) is a GNU extension that
  # BSD/macOS sed silently fails to match.
  target="$(
    fetch_stdout "https://api.github.com/repos/$REPO/releases?per_page=20" \
      | tr ',' '\n' \
      | grep -E '"(tag_name|draft)"' \
      | sed -E -e 's/.*"tag_name": *"([^"]*)".*/TAG \1/' \
               -e 's/.*"draft": *(true|false).*/DRAFT \1/' \
      | awk '
          /^TAG /   { tag = $2; have = 1; next }
          /^DRAFT / { if (have && $2 == "false") { print tag; exit } ; have = 0 }
        '
  )" || die "could not query releases for $REPO"
  [ -n "$target" ] || die "no published release found for $REPO"
fi

log "available: $target"

if [ "$FORCE" -eq 0 ] && [ -n "$current" ] && [ "$current" = "$target" ]; then
  log "already up to date"
  exit 0
fi

# ------------------------------------------------------------- delegate ------

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
local_installer="$here/install.sh"

BONSAI_VERSION="$target"
BONSAI_INSTALL_DIR="$install_dir"
export BONSAI_VERSION BONSAI_INSTALL_DIR

if [ -n "$current" ]; then
  log "updating $current -> $target"
else
  log "installing $target"
fi

if [ -f "$local_installer" ]; then
  exec sh "$local_installer"
fi

installer="$(mktemp 2>/dev/null || mktemp -t bonsai-install)"
trap 'rm -f "$installer"' EXIT INT TERM
fetch_stdout "$RAW_INSTALLER" > "$installer" || die "could not download install.sh"
sh "$installer"
