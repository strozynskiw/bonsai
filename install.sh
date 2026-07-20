#!/usr/bin/env sh
set -eu

repo="${BONSAI_REPO:-strozynskiw/bonsai}"
version="${BONSAI_VERSION:-}"
install_dir="${BONSAI_INSTALL_DIR:-$HOME/.local/bin}"

usage() {
  cat >&2 <<'EOF'
Usage:
  install.sh

Environment:
  BONSAI_VERSION      release tag to install, for example v0.2.0
  BONSAI_INSTALL_DIR  install directory, default: $HOME/.local/bin
  BONSAI_REPO         GitHub repo, default: strozynskiw/bonsai
EOF
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os:$arch" in
    Linux:x86_64|Linux:amd64)
      printf '%s\n' "x86_64-unknown-linux-gnu"
      ;;
    Darwin:arm64)
      printf '%s\n' "aarch64-apple-darwin"
      ;;
    Darwin:x86_64)
      printf '%s\n' "x86_64-apple-darwin"
      ;;
    Linux:aarch64|Linux:arm64)
      printf '%s\n' "aarch64-unknown-linux-gnu"
      ;;
    *)
      echo "unsupported platform: $os $arch" >&2
      exit 1
      ;;
  esac
}

latest_release_tag() {
  curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=1" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
    | head -n 1
}

install_binary() {
  mkdir -p "$install_dir"
  if [ -w "$install_dir" ]; then
    install -m 755 "$1" "$install_dir/bonsai"
  elif command -v sudo >/dev/null 2>&1; then
    sudo install -m 755 "$1" "$install_dir/bonsai"
  else
    echo "install directory is not writable: $install_dir" >&2
    exit 1
  fi
}

case "${1:-}" in
  -h|--help)
    usage
    exit 0
    ;;
  "")
    ;;
  *)
    echo "unexpected argument: $1" >&2
    usage
    exit 1
    ;;
esac

target="$(detect_target)"
if [ -z "$version" ]; then
  version="$(latest_release_tag)"
fi

if [ -z "$version" ]; then
  echo "could not determine a bonsai release tag" >&2
  exit 1
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

asset="bonsai-$version-$target.tar.gz"
base_url="https://github.com/$repo/releases/download/$version"

echo "Installing bonsai $version for $target"
curl -fsSL -L "$base_url/$asset" -o "$tmpdir/$asset"
curl -fsSL -L "$base_url/SHA256SUMS" -o "$tmpdir/SHA256SUMS"

(
  cd "$tmpdir"
  awk -v file="$asset" '$2 == file { print; found = 1 } END { exit found ? 0 : 1 }' \
    SHA256SUMS > "$asset.sha256"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$asset.sha256"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$asset.sha256"
  else
    echo "missing checksum verifier: install sha256sum or shasum" >&2
    exit 1
  fi

  tar -xzf "$asset"
)

install_binary "$tmpdir/bonsai"

echo "Installed $install_dir/bonsai"
"$install_dir/bonsai" --version

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *)
    echo "Note: $install_dir is not on PATH."
    ;;
esac
