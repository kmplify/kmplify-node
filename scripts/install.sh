#!/bin/sh
# kmplify-node installer.
#
#   curl -fsSL https://raw.githubusercontent.com/kmplify/kmplify-node/main/scripts/install.sh | sh
#
# Downloads the release binary for this machine, verifies its sha256 against
# the release's SHA256SUMS, installs it, and runs `kmplify-node check` so a
# broken setup is reported now rather than after the node has joined a fabric
# and started refusing jobs.
#
# Options (pass after `sh -s --` when piping):
#   --version vX.Y.Z   install a specific release (default: latest)
#   --prefix DIR       install directory (default: /usr/local/bin)
#   --service          Linux + systemd only: create the kmplify user, install
#                      the systemd unit and an env-file template, and enable
#                      the service. Requires root.
#   --no-check         skip the final `kmplify-node check`
#
# Environment:
#   KMPLIFY_INSTALL_BASE  override the asset base URL (testing/mirrors). When
#                         set, assets are fetched from exactly this URL and
#                         the GitHub release lookup is skipped.
#   GITHUB_TOKEN          used for the API lookup if set (private repo, CI).
#
# POSIX sh on purpose: a fresh VPS may not have bash.
set -eu

REPO="kmplify/kmplify-node"
PREFIX="/usr/local/bin"
VERSION=""
DO_SERVICE=0
DO_CHECK=1

say()  { printf '%s\n' "$*"; }
fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --prefix)  PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
    --service) DO_SERVICE=1; shift ;;
    --no-check) DO_CHECK=0; shift ;;
    -h|--help) sed -n '2,25p' "$0" 2>/dev/null || true; exit 0 ;;
    *) fail "unknown option: $1" ;;
  esac
done

# ---- what machine is this ------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS/$ARCH" in
  Linux/x86_64)          TARGET="x86_64-unknown-linux-musl" ;;
  Linux/aarch64|Linux/arm64) TARGET="aarch64-unknown-linux-musl" ;;
  Darwin/arm64)          TARGET="aarch64-apple-darwin" ;;
  Darwin/x86_64)         TARGET="x86_64-apple-darwin" ;;
  *) fail "unsupported platform: $OS/$ARCH. Build from source instead: cargo build --release (see README.md)" ;;
esac
ASSET="kmplify-node-${TARGET}"

# ---- where do assets come from -------------------------------------------
if [ -n "${KMPLIFY_INSTALL_BASE:-}" ]; then
  BASE="${KMPLIFY_INSTALL_BASE%/}"
  # Plaintext here means whoever is on the path chooses the binary that ends
  # up running as a service on this machine. The checksum does not help: it is
  # fetched over the same connection and would be swapped with it.
  case "$BASE" in
    https://*) ;;
    http://127.0.0.1*|http://localhost*|http://[::1]*) ;;   # local test server
    *) fail "KMPLIFY_INSTALL_BASE must be https:// (or a loopback address for testing), got: $BASE" ;;
  esac
else
  if [ -z "$VERSION" ]; then
    # No eval. The token was interpolated into a string and re-parsed by the
    # shell, so a token containing shell metacharacters executed them. curl
    # reads the header from a file instead, which also keeps it off the
    # process list where `ps` would show it.
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      hdr="$(mktemp)"
      printf 'Authorization: Bearer %s\n' "$GITHUB_TOKEN" > "$hdr"
      VERSION="$(curl -fsSL -H @"$hdr" "https://api.github.com/repos/$REPO/releases/latest" \
        | tr ',' '\n' | grep -m1 '"tag_name"' | cut -d'"' -f4)" || true
      rm -f "$hdr"
    else
      VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | tr ',' '\n' | grep -m1 '"tag_name"' | cut -d'"' -f4)" || true
    fi
    [ -n "$VERSION" ] || fail "could not resolve the latest release of $REPO. If none has been published yet, build from source (see README.md) or pass --version vX.Y.Z"
  fi
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

# ---- fetch and verify -----------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

say "fetching $ASSET ${VERSION:+($VERSION) }..."
curl -fsSL -o "$TMP/$ASSET" "$BASE/$ASSET" \
  || fail "download failed: $BASE/$ASSET"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS" \
  || fail "download failed: $BASE/SHA256SUMS (refusing to install an unverifiable binary)"

# sha256sum on Linux, shasum on macOS.
if command -v sha256sum >/dev/null 2>&1; then SHA="sha256sum";
elif command -v shasum >/dev/null 2>&1;  then SHA="shasum -a 256";
else fail "neither sha256sum nor shasum found; cannot verify the download"; fi

# ---- is SHA256SUMS itself trustworthy -------------------------------------
#
# The checksum proves the binary matches the manifest. It proves nothing about
# the MANIFEST, which arrives over the same connection from the same place: an
# attacker who can replace one replaces both, and this script would verify the
# swap against itself and report success. Checksums catch corruption; only a
# signature catches substitution.
#
# RELEASE_PUBKEY below is the release signing key, PEM, embedded in this
# script. While it is empty the signature is OPTIONAL and its absence is
# reported rather than fatal — the mechanism ships before the key exists, and
# the day the key is filled in this becomes mandatory with no other change.
# See F13 in docs/security/AUDIT-2026-08-24.md.
#
# To turn it on, generate an ECDSA P-256 key:
#
#     openssl ecparam -name prime256v1 -genkey -noout -out release-key.pem
#     openssl ec -in release-key.pem -pubout            # paste this below
#
# then add the PRIVATE key to the repository as the MINISIGN-free secret
# `RELEASE_SIGNING_KEY`, which .github/workflows/release.yml signs SHA256SUMS
# with. Keep it out of this repo and out of the release.
#
# ECDSA or RSA, NOT Ed25519 — the modern default is the wrong choice here.
# `openssl dgst -sha256 -verify` below cannot use an EdDSA key at all
# ("Explicit digest not allowed with EdDSA operations"), so signing with one
# would break every install the moment verification became mandatory.
RELEASE_PUBKEY=""

if [ -n "$RELEASE_PUBKEY" ]; then
  command -v openssl >/dev/null 2>&1 \
    || fail "openssl is required to verify the release signature; install it, or build from source"
  curl -fsSL -o "$TMP/SHA256SUMS.sig" "$BASE/SHA256SUMS.sig" \
    || fail "download failed: $BASE/SHA256SUMS.sig (this release is not signed; refusing to install)"
  printf '%s\n' "$RELEASE_PUBKEY" > "$TMP/release.pem"
  openssl dgst -sha256 -verify "$TMP/release.pem" \
    -signature "$TMP/SHA256SUMS.sig" "$TMP/SHA256SUMS" >/dev/null 2>&1 \
    || fail "SHA256SUMS does not verify under the KMPLIFY release key. Someone has \
substituted the release, or it was published without being signed. NOT installing."
  say "release signature verified"
else
  # Said out loud rather than passed over. An operator piping this into a
  # shell should know which of the two properties they are getting.
  say ""
  say "NOTE: this release is verified by CHECKSUM only, not by signature."
  say "      That detects a corrupted download, not a substituted one."
  say ""
fi

want="$(grep " $ASSET\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$want" ] || fail "SHA256SUMS has no entry for $ASSET"
got="$($SHA "$TMP/$ASSET" | cut -d' ' -f1)"
[ "$want" = "$got" ] || fail "checksum mismatch for $ASSET: expected $want, got $got. NOT installing."
say "checksum verified: $got"

# ---- install ----------------------------------------------------------------
SUDO=""
if [ ! -w "$PREFIX" ] 2>/dev/null || [ ! -d "$PREFIX" ]; then
  if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || fail "$PREFIX is not writable and sudo is unavailable; re-run as root or pass --prefix"
    SUDO="sudo"
  fi
fi
$SUDO mkdir -p "$PREFIX"
$SUDO install -m 755 "$TMP/$ASSET" "$PREFIX/kmplify-node"
say "installed $PREFIX/kmplify-node"

# ---- optional: run as a systemd service ------------------------------------
if [ "$DO_SERVICE" = 1 ]; then
  [ "$OS" = "Linux" ] || fail "--service is Linux-only (use launchd on macOS; see docs/HEADLESS-NODE.md)"
  command -v systemctl >/dev/null 2>&1 || fail "--service needs systemd"
  [ "$(id -u)" -eq 0 ] || fail "--service must run as root (it creates a user and writes to /etc)"

  curl -fsSL -o "$TMP/kmplify-node.service" "$BASE/kmplify-node.service" \
    || fail "download failed: $BASE/kmplify-node.service"
  curl -fsSL -o "$TMP/kmplify-node.env.example" "$BASE/kmplify-node.env.example" \
    || fail "download failed: $BASE/kmplify-node.env.example"

  # In the docker group only if docker exists: sessions need it, inference
  # does not, and granting docker-group on a box without docker is noise.
  if getent group docker >/dev/null 2>&1; then EXTRA="-G docker"; else EXTRA=""; fi
  id kmplify >/dev/null 2>&1 || useradd -r $EXTRA -d /var/lib/kmplify-node -s /usr/sbin/nologin kmplify

  install -m 644 "$TMP/kmplify-node.service" /etc/systemd/system/kmplify-node.service
  # Never overwrite an existing config: the env file is where the operator's
  # ceilings and opt-ins live.
  [ -f /etc/kmplify-node.env ] || install -m 640 "$TMP/kmplify-node.env.example" /etc/kmplify-node.env
  systemctl daemon-reload
  say "service installed. Review /etc/kmplify-node.env, then: systemctl enable --now kmplify-node"
fi

# ---- preflight ---------------------------------------------------------------
if [ "$DO_CHECK" = 1 ]; then
  say ""
  say "running preflight (connects to nothing):"
  "$PREFIX/kmplify-node" check || {
    say ""
    say "preflight reported problems (above). The binary is installed; fix the"
    say "configuration it points at, then run: kmplify-node check"
    exit 1
  }
fi
say ""
say "done. Start lending with: kmplify-node   (or the systemd service)"
say "watch and control it with: kmplify-node tui"
