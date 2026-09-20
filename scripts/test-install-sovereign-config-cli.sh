#!/bin/sh
# Exercise install-sovereign-config-cli.sh without touching the network.
#
# The script decides which binary the deploy step will run, from a JSON document
# it fetches at deploy time. Everything interesting is in that decision — picking
# the *cli* installer out of a directory that also publishes the *mcp* one with a
# near-identical name, and refusing to install anything when the download is
# wrong. The pipeline's own deploy step only ever exercises the happy path, and
# only against whatever the server happens to be serving that day, so none of the
# refusals are covered there.
#
# A stub `wget` on PATH serves a fixture directory, which keeps the real parsing,
# checksum and install logic under test while the transport is faked — the same
# shape as the `docker` stub in test-deploy-v-note.sh.
#
# POSIX sh, not bash: this runs in the `docker:27-cli` CI image (see
# .woodpecker/checks.yml, deploy-script-validation), which ships no bash.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/scripts/install-sovereign-config-cli.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
pass() { echo "  ok   — $1"; }
fail() { echo "  FAIL — $1" >&2; FAILURES=$((FAILURES + 1)); }

# --- stub wget -------------------------------------------------------------
# Mirrors the busybox invocation the script uses (`wget -q -T 30 -O dest url`)
# and resolves the URL's last path segment inside $WGET_FIXTURES. A name with no
# fixture exits non-zero, which is what an HTTP error looks like to the script.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/wget" <<'STUB'
#!/bin/sh
dest=""; url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -O) dest="$2"; shift 2 ;;
    -T) shift 2 ;;
    -q) shift ;;
    *)  url="$1"; shift ;;
  esac
done
src="$WGET_FIXTURES/${url##*/}"
[ -f "$src" ] || exit 1
cp "$src" "$dest"
STUB
chmod +x "$WORK/bin/wget"

# --- fixtures --------------------------------------------------------------
# A fake self-extracting installer. The real one installs a 3MB static binary;
# all this test needs is something that honours SOVEREIGN_CONFIG_BIN and records
# which installer ran, so the "picked the cli, not the mcp" assertion has teeth.
make_installer() { # <fixture-dir> <installer-name>
  cat > "$1/$2" <<INSTALLER
#!/bin/sh
set -eu
echo "$2" > "\$INSTALL_MARKER"
mkdir -p "\$SOVEREIGN_CONFIG_BIN"
cat > "\$SOVEREIGN_CONFIG_BIN/sovereign-config" <<'BIN'
#!/bin/sh
echo "sovereign-config 9.9.9-test"
BIN
chmod +x "\$SOVEREIGN_CONFIG_BIN/sovereign-config"
INSTALLER
  # The real server publishes "<digest>  <release filename>"; the script reads
  # field 1 only, because it saves the download under a local name of its own.
  printf '%s  %s\n' "$(sha256sum "$1/$2" | awk '{print $1}')" "$2" > "$1/$2.sha256"
}

CLI=install-sovereign-config-cli-2.26.2-x86_64-linux.sh
MCP=install-sovereign-config-mcp-2.26.2-x86_64-linux.sh

# The live manifest's exact shape, as served today (single line, no whitespace).
manifest_entry() { printf '{"file":"%s","checksum":"%s.sha256","size":3107915}' "$1" "$1"; }

# --- harness ---------------------------------------------------------------
# Each case gets a fresh fixture dir and a fresh, empty install dir, so
# "nothing was installed" is a statement about this case alone.
CASE=0
new_case() {
  CASE=$((CASE + 1))
  FIXTURES="$WORK/fixtures-$CASE"
  BINDIR="$WORK/bindir-$CASE"
  MARKER="$WORK/marker-$CASE"
  mkdir -p "$FIXTURES" "$BINDIR"
}

# Runs the script with the stub on PATH and the install dir *also* on PATH, so
# the script's own "is it on PATH afterwards" check is exercised rather than
# sidestepped. Output is captured; cases assert on the exit status.
run_install() {
  (
    export PATH="$WORK/bin:$BINDIR:$PATH"
    export WGET_FIXTURES="$FIXTURES"
    export SOVEREIGN_CONFIG_BIN="$BINDIR"
    export SOVEREIGN_CONFIG_DIST_URL="https://example.invalid/dist"
    export INSTALL_MARKER="$MARKER"
    "$SCRIPT"
  ) > "$WORK/out-$CASE" 2>&1
}

installed() { [ -x "$BINDIR/sovereign-config" ]; }

# --- cases -----------------------------------------------------------------
echo "install-sovereign-config-cli.sh"

# 1. The whole point: two installers published side by side, same prefix, same
#    suffix, and only the cli one is ours.
new_case
make_installer "$FIXTURES" "$CLI"
make_installer "$FIXTURES" "$MCP"
printf '{"installers":[%s,%s]}' "$(manifest_entry "$CLI")" "$(manifest_entry "$MCP")" \
  > "$FIXTURES/manifest.json"
if run_install; then
  if ! installed; then
    fail "two-installer manifest: exited 0 but installed nothing"
  elif [ "$(cat "$MARKER")" != "$CLI" ]; then
    fail "two-installer manifest: ran $(cat "$MARKER"), expected $CLI"
  elif ! grep -q '9.9.9-test' "$WORK/out-$CASE"; then
    fail "two-installer manifest: did not log the installed version"
  else
    pass "picks the cli installer out of a cli+mcp manifest and logs its version"
  fi
else
  fail "two-installer manifest: exited non-zero ($(tail -1 "$WORK/out-$CASE"))"
fi

# 2. A truncated or mangled download is the failure this checksum exists for.
new_case
make_installer "$FIXTURES" "$CLI"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$CLI" \
  > "$FIXTURES/$CLI.sha256"
printf '{"installers":[%s]}' "$(manifest_entry "$CLI")" > "$FIXTURES/manifest.json"
if run_install; then
  fail "checksum mismatch: exited 0"
elif installed; then
  fail "checksum mismatch: installed anyway"
else
  pass "checksum mismatch fails and installs nothing"
fi

# 3. The server has stopped publishing a cli installer (or only the mcp one is
#    there). Fail loudly rather than fall back to the wrong binary.
new_case
make_installer "$FIXTURES" "$MCP"
printf '{"installers":[%s]}' "$(manifest_entry "$MCP")" > "$FIXTURES/manifest.json"
if run_install; then
  fail "mcp-only manifest: exited 0"
elif installed; then
  fail "mcp-only manifest: installed the mcp binary"
else
  pass "a manifest with no cli entry fails and installs nothing"
fi

# 4. Server down / DNS gone / 404 — the deploy must not continue without a CLI.
new_case
if run_install; then
  fail "unreachable manifest: exited 0"
elif installed; then
  fail "unreachable manifest: installed something"
else
  pass "an unreachable manifest fails and installs nothing"
fi

# 5. The installer exists but its checksum file does not.
new_case
make_installer "$FIXTURES" "$CLI"
rm -f "$FIXTURES/$CLI.sha256"
printf '{"installers":[%s]}' "$(manifest_entry "$CLI")" > "$FIXTURES/manifest.json"
if run_install; then
  fail "missing checksum file: exited 0"
elif installed; then
  fail "missing checksum file: installed anyway"
else
  pass "a missing checksum file fails and installs nothing"
fi

# 6. Both names are pasted into a URL and used as local filenames, and both come
#    from the network. A manifest that names a path must not be followed.
new_case
make_installer "$FIXTURES" "$CLI"
printf '{"installers":[{"file":"install-sovereign-config-cli-../../etc/x-x86_64-linux.sh","checksum":"x.sha256","size":1}]}' \
  > "$FIXTURES/manifest.json"
if run_install; then
  fail "path in manifest: exited 0"
elif installed; then
  fail "path in manifest: installed anyway"
elif ! grep -q 'names a path' "$WORK/out-$CASE"; then
  fail "path in manifest: failed for the wrong reason ($(tail -1 "$WORK/out-$CASE"))"
else
  pass "a manifest naming a path rather than a filename is refused"
fi

# 7. Two cli installers means the server changed its publishing contract; picking
#    one at random is how a deploy silently downgrades.
new_case
make_installer "$FIXTURES" "$CLI"
OLD=install-sovereign-config-cli-2.25.0-x86_64-linux.sh
make_installer "$FIXTURES" "$OLD"
printf '{"installers":[%s,%s]}' "$(manifest_entry "$CLI")" "$(manifest_entry "$OLD")" \
  > "$FIXTURES/manifest.json"
if run_install; then
  fail "two cli installers: exited 0"
elif installed; then
  fail "two cli installers: installed anyway"
else
  pass "two cli installers are refused rather than guessed between"
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "$FAILURES case(s) failed" >&2
  exit 1
fi
echo "all cases passed"
