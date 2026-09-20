#!/bin/sh
set -eu

# Install the sovereign-config CLI into the deploy step, so the step can read its
# configuration with `sovereign-config render` instead of having it brokered in as
# Woodpecker secrets (#391).
#
# There is no CLI container image: the running sovereign-config server publishes a
# self-extracting installer under /dist, and that is the only distribution channel.
#
# PINNED, like every other external artifact this repo consumes — the CI images
# and both Woodpecker plugins are `@sha256:` digest pins, and this is the same
# thing spelled for a file: an exact version, and the digest it must hash to.
#
# The alternative considered (#391 decision 5) was to discover the current
# filename from /dist/manifest.json and install whatever the server currently
# publishes. That was rejected at pickup: #391 migrates where the deploy's
# configuration comes from and should change nothing else, and a CLI that
# silently tracks the server is new behaviour — a moving dependency in the deploy
# path. When the server is upgraded this 404s and the deploy fails loudly, which
# is the same bargain every other pin in this repo makes. Bump both constants
# together, deliberately, as a commit.
#
# The digest is pinned here rather than read from the published `.sha256`
# alongside the installer, for the same reason a digest pin beats a tag: a
# checksum the server hands over with the file it describes attests nothing. This
# way the trust anchor is in git and CI verifies the download against it.
#
# Runs in `docker:27-cli` (Alpine): busybox `wget` and `sha256sum` only — no
# `curl`, no `jq`. POSIX sh for the same reason (no bash).
#
# Environment:
#   SOVEREIGN_CONFIG_DIST_URL  base URL of the /dist directory (default: live server)
#   SOVEREIGN_CONFIG_BIN       install directory (default: /usr/local/bin, on PATH
#                              in the CI image — the installer's own default is
#                              ~/.local/bin, which is not)

# Keep in lockstep with the running server; see docs/DEPLOY.md.
CLI_VERSION="2.26.2"
CLI_SHA256="b05aab9cbca4952bcaea4f2213241b468987b50fb98a26373c881cad485869d8"

DIST_URL="${SOVEREIGN_CONFIG_DIST_URL:-https://sovereign-config.desync.link/dist}"
SOVEREIGN_CONFIG_BIN="${SOVEREIGN_CONFIG_BIN:-/usr/local/bin}"
export SOVEREIGN_CONFIG_BIN

installer="install-sovereign-config-cli-${CLI_VERSION}-x86_64-linux.sh"

fail() {
  echo "install-sovereign-config-cli: $1" >&2
  exit 1
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

echo "==> downloading $installer"
# busybox wget exits non-zero on an HTTP error, so a 404 — which is what a server
# upgrade past the pinned version looks like — fails here with the URL named.
wget -q -T 30 -O "$work/installer.sh" "$DIST_URL/$installer" ||
  fail "download failed: $DIST_URL/$installer (has the server moved past $CLI_VERSION? bump CLI_VERSION and CLI_SHA256 together)"

actual="$(sha256sum "$work/installer.sh" | awk '{print $1}')"
[ "$actual" = "$CLI_SHA256" ] ||
  fail "checksum mismatch for $installer (expected $CLI_SHA256, got $actual)"

# Everything above is verification; this is the first line with a side effect.
# The installer verifies its own payload and installs to $SOVEREIGN_CONFIG_BIN.
sh "$work/installer.sh" || fail "installer failed"

command -v sovereign-config >/dev/null 2>&1 ||
  fail "sovereign-config is not on PATH after install (SOVEREIGN_CONFIG_BIN=$SOVEREIGN_CONFIG_BIN)"

# Assign first, echo second. Inside `echo "$(...)"` the substitution's exit
# status is discarded, so a binary that installs but cannot execute — a noexec
# mount, an architecture mismatch — would print an empty version and exit 0,
# leaving the deploy to fail later at `render` with an error pointing at the
# wrong step. `command -v` above only proves a *file* is on PATH.
version="$(sovereign-config --version)" ||
  fail "installed sovereign-config does not run (SOVEREIGN_CONFIG_BIN=$SOVEREIGN_CONFIG_BIN)"

echo "==> installed $version"
