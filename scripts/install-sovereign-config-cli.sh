#!/bin/sh
set -eu

# Install the sovereign-config CLI into the deploy step, so the step can read its
# configuration with `sovereign-config render` instead of having it brokered in as
# Woodpecker secrets (#391).
#
# There is no CLI container image: the running sovereign-config server publishes a
# self-extracting installer under /dist, and that is the only distribution channel.
#
# The installer name is *discovered*, never pinned. The server serves only its own
# release's installer — /dist/manifest.json lists exactly the current version and
# nothing older — so a pinned name (`…-cli-2.26.2-…`) starts 404ing the moment the
# server is upgraded, and takes v-note's deploy down with it at the next push. The
# manifest always names the installer that matches the server we are about to talk
# to, which is also the protocol version the CLI has to speak.
#
# Discovery adds no trust root. The manifest, the installer and its checksum all
# come from the same origin that holds every secret this deploy is about to read;
# if that origin is lying, pinning a filename saves nothing. The checksum is
# verified anyway, because it costs one command and catches the ordinary failure —
# a truncated or proxy-mangled download, not an attacker.
#
# Runs in `docker:27-cli` (Alpine): busybox `wget`, `sha256sum`, `awk`, `sed` and
# `tar` only — no `curl`, no `jq`. POSIX sh for the same reason (no bash).
#
# Environment:
#   SOVEREIGN_CONFIG_DIST_URL  base URL of the /dist directory (default: live server)
#   SOVEREIGN_CONFIG_BIN       install directory (default: /usr/local/bin, on PATH
#                              in the CI image — the installer's own default is
#                              ~/.local/bin, which is not)

DIST_URL="${SOVEREIGN_CONFIG_DIST_URL:-https://sovereign-config.desync.link/dist}"
SOVEREIGN_CONFIG_BIN="${SOVEREIGN_CONFIG_BIN:-/usr/local/bin}"
export SOVEREIGN_CONFIG_BIN

fail() {
  echo "install-sovereign-config-cli: $1" >&2
  exit 1
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

# busybox wget exits non-zero on an HTTP error, so a 404 or an unreachable host
# fails here rather than leaving a stub file to be parsed as JSON further down.
fetch() {
  wget -q -T 30 -O "$2" "$1" || fail "download failed: $1"
}

echo "==> fetching installer manifest from $DIST_URL"
fetch "$DIST_URL/manifest.json" "$work/manifest.json"

# No jq in this image, and the manifest is a single line, so split the array into
# one object per line first and let grep pick the row. Matching on the whole
# `"file":"…"` shape rather than a bare substring is what keeps the *mcp* installer
# — same prefix, same suffix, published alongside — out of the result.
entry="$(sed 's/},[[:space:]]*{/}\
{/g' "$work/manifest.json" \
  | grep '"file"[[:space:]]*:[[:space:]]*"install-sovereign-config-cli-[^"]*-x86_64-linux\.sh"' \
  || true)"

[ -n "$entry" ] ||
  fail "no x86_64-linux CLI installer in $DIST_URL/manifest.json"
[ "$(printf '%s\n' "$entry" | wc -l)" -eq 1 ] ||
  fail "manifest lists more than one x86_64-linux CLI installer; refusing to guess"

installer="$(printf '%s\n' "$entry" | sed -n 's/.*"file"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
checksum="$(printf '%s\n' "$entry" | sed -n 's/.*"checksum"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"

[ -n "$installer" ] || fail "manifest entry has no \"file\""
[ -n "$checksum" ] || fail "manifest entry has no \"checksum\""

# Both names are pasted straight into a URL and used as local filenames. They come
# from a JSON document off the network, so keep them to bare filenames: a `/` or a
# `..` would otherwise let the manifest point the download somewhere else entirely.
for name in "$installer" "$checksum"; do
  case "$name" in
    */* | *..*) fail "manifest names a path, not a filename: $name" ;;
  esac
done

echo "==> installing $installer"
fetch "$DIST_URL/$installer" "$work/installer.sh"
fetch "$DIST_URL/$checksum" "$work/installer.sha256"

# Compare digests rather than `sha256sum -c`: the published file names the
# installer by its release filename, and ours is saved under a fixed local name.
expected="$(awk '{print $1; exit}' "$work/installer.sha256")"
actual="$(sha256sum "$work/installer.sh" | awk '{print $1}')"

case "$expected" in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*)
    [ "${#expected}" -eq 64 ] || fail "published checksum is not a sha256 digest: $checksum" ;;
  *) fail "published checksum is not a sha256 digest: $checksum" ;;
esac

[ "$expected" = "$actual" ] ||
  fail "checksum mismatch for $installer (expected $expected, got $actual)"

# Everything above is verification; this is the first line with a side effect.
# The installer verifies its own payload, installs to $SOVEREIGN_CONFIG_BIN, and
# refuses to report success unless the installed binary actually runs.
sh "$work/installer.sh" || fail "installer failed"

command -v sovereign-config >/dev/null 2>&1 ||
  fail "sovereign-config is not on PATH after install (SOVEREIGN_CONFIG_BIN=$SOVEREIGN_CONFIG_BIN)"

# The audit trail: which CLI actually ran this deploy. Cheap, and the one thing
# that is not reconstructable from the commit once the server has moved on.
echo "==> installed $(sovereign-config --version)"
