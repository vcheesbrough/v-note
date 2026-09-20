#!/bin/sh
# Exercise install-sovereign-config-cli.sh without touching the network.
#
# The script puts a binary on PATH that the deploy then runs with the database
# password and both access URLs in its environment, so what it refuses to install
# matters more than what it installs. The pipeline's own deploy step only ever
# exercises the happy path, against whatever the server is serving that day, so
# none of the refusals are covered there.
#
# A stub `wget` on PATH serves a fixture directory, which keeps the real checksum
# and install logic under test while the transport is faked — the same shape as
# the `docker` stub in test-deploy-v-note.sh.
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

# The pinned constants are the contract; read them from the script so the cases
# below cannot drift from it, and so a bump has to be deliberate in one place.
CLI_VERSION="$(sed -n 's/^CLI_VERSION="\(.*\)"$/\1/p' "$SCRIPT")"
CLI_SHA256="$(sed -n 's/^CLI_SHA256="\(.*\)"$/\1/p' "$SCRIPT")"
CLI="install-sovereign-config-cli-${CLI_VERSION}-x86_64-linux.sh"

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
# A fake self-extracting installer. The real one carries a 3MB static binary;
# these cases only need something that honours SOVEREIGN_CONFIG_BIN. `payload`
# decides what the installed `sovereign-config` does when run.
make_installer() { # <fixture-dir> <installer-name> <payload>
  cat > "$1/$2" <<INSTALLER
#!/bin/sh
set -eu
mkdir -p "\$SOVEREIGN_CONFIG_BIN"
cat > "\$SOVEREIGN_CONFIG_BIN/sovereign-config" <<'BIN'
#!/bin/sh
$3
BIN
chmod +x "\$SOVEREIGN_CONFIG_BIN/sovereign-config"
INSTALLER
}

# --- harness ---------------------------------------------------------------
# Each case gets a fresh fixture dir and a fresh, empty install dir, so
# "nothing was installed" is a statement about this case alone.
CASE=0
new_case() {
  CASE=$((CASE + 1))
  FIXTURES="$WORK/fixtures-$CASE"
  BINDIR="$WORK/bindir-$CASE"
  mkdir -p "$FIXTURES" "$BINDIR"
}

# Runs the script with the stub on PATH and the install dir *also* on PATH, so
# the script's own "is it on PATH afterwards" check is exercised rather than
# sidestepped.
run_install() {
  (
    export PATH="$WORK/bin:$BINDIR:$PATH"
    export WGET_FIXTURES="$FIXTURES"
    export SOVEREIGN_CONFIG_BIN="$BINDIR"
    export SOVEREIGN_CONFIG_DIST_URL="https://example.invalid/dist"
    "$SCRIPT"
  ) > "$WORK/out-$CASE" 2>&1
}

installed() { [ -x "$BINDIR/sovereign-config" ]; }

# Rewrites a fixture's digest into the script's pinned value, so the happy-path
# cases stay green across a deliberate version/digest bump.
pin_to_fixture() {
  sed -i "s/^CLI_SHA256=\".*\"\$/CLI_SHA256=\"$(sha256sum "$1" | awk '{print $1}')\"/" "$2"
}

# --- cases -----------------------------------------------------------------
echo "install-sovereign-config-cli.sh"

# The pin is the whole design (#391): it is what stops the deploy silently
# tracking the server. Assert it is a real version and a real digest, not a
# placeholder someone left behind.
new_case
if ! echo "$CLI_VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  fail "CLI_VERSION is not a semver: '$CLI_VERSION'"
elif ! echo "$CLI_SHA256" | grep -qE '^[0-9a-f]{64}$'; then
  fail "CLI_SHA256 is not a sha256 digest: '$CLI_SHA256'"
else
  pass "the CLI version and digest are both pinned to concrete values"
fi

# The happy path, with the pinned digest honoured.
new_case
make_installer "$FIXTURES" "$CLI" 'echo "sovereign-config 9.9.9-test"'
SCRIPT_COPY="$WORK/script-$CASE.sh"
cp "$SCRIPT" "$SCRIPT_COPY"
pin_to_fixture "$FIXTURES/$CLI" "$SCRIPT_COPY"
( export PATH="$WORK/bin:$BINDIR:$PATH" WGET_FIXTURES="$FIXTURES" \
    SOVEREIGN_CONFIG_BIN="$BINDIR" SOVEREIGN_CONFIG_DIST_URL="https://example.invalid/dist"
  sh "$SCRIPT_COPY" ) > "$WORK/out-$CASE" 2>&1 && ok=1 || ok=0
if [ "$ok" -ne 1 ]; then
  fail "happy path: exited non-zero ($(tail -1 "$WORK/out-$CASE"))"
elif ! installed; then
  fail "happy path: exited 0 but installed nothing"
elif ! grep -q '9.9.9-test' "$WORK/out-$CASE"; then
  fail "happy path: did not log the installed version"
else
  pass "installs the pinned installer and logs the version it got"
fi

# The digest is the trust anchor. A file that is not the pinned one — a tampered
# or truncated download — must not be installed.
new_case
make_installer "$FIXTURES" "$CLI" 'echo "sovereign-config 9.9.9-test"'
if run_install; then
  fail "digest mismatch: exited 0"
elif installed; then
  fail "digest mismatch: installed anyway"
elif ! grep -q 'checksum mismatch' "$WORK/out-$CASE"; then
  fail "digest mismatch: failed for the wrong reason ($(tail -1 "$WORK/out-$CASE"))"
else
  pass "an installer that does not match the pinned digest is refused"
fi

# What a server upgrade past the pin looks like: the pinned filename is gone.
# Must fail loudly, and say what to do about it.
new_case
if run_install; then
  fail "pinned installer missing: exited 0"
elif installed; then
  fail "pinned installer missing: installed something"
elif ! grep -q 'bump CLI_VERSION and CLI_SHA256' "$WORK/out-$CASE"; then
  fail "pinned installer missing: error does not say how to fix it"
else
  pass "a 404 on the pinned installer fails with the bump instruction"
fi

# `command -v` only proves a file exists. A binary that installs but cannot run
# — noexec mount, wrong architecture — must not be reported as success, or the
# deploy dies later at `render` pointing at the wrong step.
new_case
make_installer "$FIXTURES" "$CLI" 'exit 1'
SCRIPT_COPY="$WORK/script-$CASE.sh"
cp "$SCRIPT" "$SCRIPT_COPY"
pin_to_fixture "$FIXTURES/$CLI" "$SCRIPT_COPY"
( export PATH="$WORK/bin:$BINDIR:$PATH" WGET_FIXTURES="$FIXTURES" \
    SOVEREIGN_CONFIG_BIN="$BINDIR" SOVEREIGN_CONFIG_DIST_URL="https://example.invalid/dist"
  sh "$SCRIPT_COPY" ) > "$WORK/out-$CASE" 2>&1 && ok=1 || ok=0
if [ "$ok" -eq 1 ]; then
  fail "unrunnable binary: exited 0"
elif ! grep -q 'does not run' "$WORK/out-$CASE"; then
  fail "unrunnable binary: failed for the wrong reason ($(tail -1 "$WORK/out-$CASE"))"
else
  pass "an installed binary that cannot execute fails the step"
fi

# The other half of "do not report success without proving it": an installer that
# exits 0 having installed nothing at all. `command -v` is the only thing between
# that and a green step, and every other fixture here writes the binary, so
# without this case that branch is never taken.
new_case
cat > "$FIXTURES/$CLI" <<'INSTALLER'
#!/bin/sh
exit 0
INSTALLER
SCRIPT_COPY="$WORK/script-$CASE.sh"
cp "$SCRIPT" "$SCRIPT_COPY"
pin_to_fixture "$FIXTURES/$CLI" "$SCRIPT_COPY"
( export PATH="$WORK/bin:$BINDIR:$PATH" WGET_FIXTURES="$FIXTURES" \
    SOVEREIGN_CONFIG_BIN="$BINDIR" SOVEREIGN_CONFIG_DIST_URL="https://example.invalid/dist"
  sh "$SCRIPT_COPY" ) > "$WORK/out-$CASE" 2>&1 && ok=1 || ok=0
if [ "$ok" -eq 1 ]; then
  fail "installer installed nothing: exited 0"
elif installed; then
  fail "installer installed nothing: but a binary appeared"
elif ! grep -q 'not on PATH after install' "$WORK/out-$CASE"; then
  fail "installer installed nothing: failed for the wrong reason ($(tail -1 "$WORK/out-$CASE"))"
else
  pass "an installer that exits 0 without installing anything fails the step"
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "$FAILURES case(s) failed" >&2
  exit 1
fi
echo "all cases passed"
