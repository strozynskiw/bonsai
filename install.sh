#!/bin/sh
# Install or upgrade Bonsai from a signed GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/strozynskiw/bonsai/master/install.sh | sh
#
# Environment:
#   BONSAI_VERSION      Tag to install (e.g. v0.2.0-rc.2). Default: newest release.
#   BONSAI_INSTALL_DIR  Install directory. Default: $HOME/.local/bin
#   BONSAI_SKIP_SIGNATURE=1
#                       Install with checksum-only verification when the local
#                       OpenSSL cannot verify Ed25519. Weaker: see verify_manifest.
#
# Verification chain (each step gates the next):
#   1. release-manifest.json is Ed25519-verified against the key baked in below.
#   2. The archive's SHA-256 must match the digest recorded in that manifest.
#   3. The extracted binary's SHA-256 must match the manifest's binary digest.
# Checksums are only meaningful because the manifest carrying them is signed;
# never "simplify" this by trusting SHA256SUMS on its own, since an attacker who
# can replace the archive can replace an unsigned checksum file alongside it.

set -eu

REPO="strozynskiw/bonsai"

# Base64 of the raw 32-byte Ed25519 public key — the same value published as the
# repository variable BONSAI_RELEASE_PUBLIC_KEY and validated by the release
# workflow before it signs anything. Empty until the release keypair exists.
BONSAI_RELEASE_PUBLIC_KEY="6xBgRQclY7/poBF5yo4xsqqcbEl1IxD8qIV3eaGFInI="

INSTALL_DIR="${BONSAI_INSTALL_DIR:-$HOME/.local/bin}"
TMPDIR_BONSAI=""

log() { printf 'bonsai: %s\n' "$1" >&2; }
die() { printf 'bonsai: error: %s\n' "$1" >&2; exit 1; }

cleanup() {
  [ -n "$TMPDIR_BONSAI" ] && rm -rf "$TMPDIR_BONSAI"
}
trap cleanup EXIT INT TERM

need() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# ---------------------------------------------------------------- platform ---
# Release targets: x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) die "unsupported operating system: $os (build from source: cargo install --git https://github.com/$REPO)" ;;
  esac
  case "$arch" in
    x86_64|amd64)  arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *) die "unsupported architecture: $arch (build from source: cargo install --git https://github.com/$REPO)" ;;
  esac
  printf '%s-%s' "$arch_part" "$os_part"
}

# ----------------------------------------------------------------- network ---

fetch() {
  # fetch <url> <output-path>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$1" -O "$2"
  else
    die "need curl or wget to download releases"
  fi
}

# The release workflow publishes with --prerelease, and GitHub's
# /releases/latest endpoint skips prereleases entirely — it would 404 here.
# Enumerate instead and take the newest non-draft entry.
#
# sed -E is deliberate: the BRE spelling of alternation (\|) is a GNU
# extension that BSD/macOS sed silently fails to match.
resolve_version() {
  if [ -n "${BONSAI_VERSION:-}" ]; then
    printf '%s' "$BONSAI_VERSION"
    return
  fi
  releases="$TMPDIR_BONSAI/releases.json"
  fetch "https://api.github.com/repos/$REPO/releases?per_page=20" "$releases" \
    || die "could not query releases for $REPO"
  tag="$(
    tr ',' '\n' < "$releases" \
      | grep -E '"(tag_name|draft)"' \
      | sed -E -e 's/.*"tag_name": *"([^"]*)".*/TAG \1/' \
               -e 's/.*"draft": *(true|false).*/DRAFT \1/' \
      | awk '
          /^TAG /   { tag = $2; have = 1; next }
          /^DRAFT / { if (have && $2 == "false") { print tag; exit } ; have = 0 }
        '
  )"
  [ -n "$tag" ] || die "no published release found for $REPO"
  printf '%s' "$tag"
}

# ------------------------------------------------------------ verification ---

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "need sha256sum or shasum to verify downloads"
  fi
}

# Ed25519-verify release-manifest.json against the baked-in public key.
#
# The SPKI DER prefix for Ed25519 is a fixed 12 bytes, and 12 is divisible by 3,
# so its base64 ("MCowBQYDK2VwAyEA") concatenates cleanly with the base64 of the
# raw 32-byte key to form a valid PEM body. That avoids assembling DER with
# xxd/od, which are not reliably present.
verify_manifest() {
  manifest="$1"
  signature="$2"

  if [ "${BONSAI_SKIP_SIGNATURE:-}" = "1" ]; then
    log "WARNING: skipping signature verification (BONSAI_SKIP_SIGNATURE=1)."
    log "WARNING: checksums will be taken from an unverified manifest."
    return 0
  fi

  [ -n "$BONSAI_RELEASE_PUBLIC_KEY" ] || die \
"no release public key is baked into this installer, so the download cannot be
  verified. This installer predates the project's release keypair. Re-download
  the current installer, or set BONSAI_SKIP_SIGNATURE=1 to accept unverified
  checksums."

  need openssl

  pem="$TMPDIR_BONSAI/release-key.pem"
  {
    printf -- '-----BEGIN PUBLIC KEY-----\n'
    printf 'MCowBQYDK2VwAyEA%s\n' "$BONSAI_RELEASE_PUBLIC_KEY"
    printf -- '-----END PUBLIC KEY-----\n'
  } > "$pem"

  raw_sig="$TMPDIR_BONSAI/release-manifest.sig.bin"
  base64 -d < "$signature" > "$raw_sig" 2>/dev/null \
    || base64 -D < "$signature" > "$raw_sig" 2>/dev/null \
    || die "could not decode release signature"

  # -rawin is required for Ed25519 and is unsupported by LibreSSL, which is what
  # /usr/bin/openssl is on macOS. Fail with a route forward rather than silently
  # downgrading to unsigned checksums.
  if ! openssl pkeyutl -verify -rawin -pubin -inkey "$pem" \
        -in "$manifest" -sigfile "$raw_sig" >/dev/null 2>&1; then
    if ! openssl pkeyutl -help 2>&1 | grep -q -- '-rawin'; then
      die \
"this OpenSSL cannot verify Ed25519 signatures (-rawin unsupported; macOS ships
  LibreSSL). Install a real OpenSSL — 'brew install openssl@3' — and re-run, or
  set BONSAI_SKIP_SIGNATURE=1 to accept unverified checksums."
    fi
    die "release manifest signature is INVALID — refusing to install"
  fi
  log "verified release signature"
}

# Pull one field out of the compact, deterministic manifest JSON without jq.
# Asset objects are emitted with keys in a fixed order:
#   {"archive":..,"archive_sha256":..,"binary_sha256":..,"target":..}
manifest_field() {
  # manifest_field <manifest> <target> <field>
  tr '{' '\n' < "$1" \
    | grep "\"target\":\"$2\"" \
    | sed -n "s/.*\"$3\":\"\([^\"]*\)\".*/\1/p" \
    | head -n1
}

# --------------------------------------------------------------- main flow ---

main() {
  need tar
  need sed
  need awk

  TMPDIR_BONSAI="$(mktemp -d 2>/dev/null || mktemp -d -t bonsai)"

  target="$(detect_target)"
  tag="$(resolve_version)"
  log "installing $tag ($target)"

  base="https://github.com/$REPO/releases/download/$tag"
  manifest="$TMPDIR_BONSAI/release-manifest.json"
  signature="$TMPDIR_BONSAI/release-manifest.json.sig"

  fetch "$base/release-manifest.json" "$manifest" \
    || die "could not download release manifest for $tag"
  if [ "${BONSAI_SKIP_SIGNATURE:-}" != "1" ]; then
    fetch "$base/release-manifest.json.sig" "$signature" \
      || die "could not download release signature for $tag"
  fi

  verify_manifest "$manifest" "$signature"

  archive_name="$(manifest_field "$manifest" "$target" archive)"
  [ -n "$archive_name" ] || die "release $tag has no build for $target"
  want_archive="$(manifest_field "$manifest" "$target" archive_sha256)"
  want_binary="$(manifest_field "$manifest" "$target" binary_sha256)"

  archive="$TMPDIR_BONSAI/$archive_name"
  fetch "$base/$archive_name" "$archive" || die "could not download $archive_name"

  got_archive="$(sha256_of "$archive")"
  [ "$got_archive" = "$want_archive" ] \
    || die "archive checksum mismatch (expected $want_archive, got $got_archive)"

  tar -xzf "$archive" -C "$TMPDIR_BONSAI" \
    || die "could not extract $archive_name"
  binary="$TMPDIR_BONSAI/bonsai"
  [ -f "$binary" ] || die "archive did not contain a bonsai binary"

  got_binary="$(sha256_of "$binary")"
  [ "$got_binary" = "$want_binary" ] \
    || die "binary checksum mismatch (expected $want_binary, got $got_binary)"
  log "verified checksums"

  mkdir -p "$INSTALL_DIR" || die "could not create $INSTALL_DIR"
  chmod 0755 "$binary"
  # Replace via rename so a running bonsai is never a half-written file.
  mv -f "$binary" "$INSTALL_DIR/bonsai" \
    || die "could not install into $INSTALL_DIR (try BONSAI_INSTALL_DIR=...)"

  log "installed $tag to $INSTALL_DIR/bonsai"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      log ""
      log "$INSTALL_DIR is not on your PATH. Add it:"
      log "    export PATH=\"$INSTALL_DIR:\$PATH\""
      ;;
  esac
}

main "$@"
