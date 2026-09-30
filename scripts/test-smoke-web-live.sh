#!/usr/bin/env bash
# Test scripts/smoke-web-live.sh without a deployment, an IdP or a browser.
#
# Why this exists: like smoke-oidc-login.sh, the live web smoke (#179) only ever
# runs post-deploy against shared infrastructure, so a broken driver would be
# found by the next deploy rather than by CI. The product assertions live in the
# Playwright spec (e2e/live/); what the *script* owns is its inputs and how it
# hands over — and one property that matters more than the rest: it must never
# print the password.
#
# `npm` and `npx` are replaced on PATH by stubs that record what they were asked
# to run, and the app's /health by a local stub server:
#
#   missing-*   → a required variable is unset       → fails before running anything
#   app-down    → /health never answers 2xx          → fails, spec never runs
#   healthy     → runs `npm ci`, then the live config against BASE_URL → passes
#   spec-fails  → the spec exits non-zero            → the script exits non-zero
#
# and in every mode the password must not appear anywhere in the output.

set -euo pipefail

cd "$(dirname "$0")/.."

PORT="${TEST_PORT:-18179}"
WORK=$(mktemp -d)
STUB_PID=""
PASSWORD="not-a-real-password-$RANDOM$RANDOM"
failures=0

cleanup() {
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$WORK/bin"
# The stubs record argv and the environment the spec would see (minus the
# password's value), and exit with whatever the test asks for.
cat > "$WORK/bin/npm" <<'STUB'
#!/usr/bin/env bash
echo "npm $*" >> "$STUB_LOG"
STUB
cat > "$WORK/bin/npx" <<'STUB'
#!/usr/bin/env bash
{
  echo "npx $*"
  echo "cwd=$(basename "$PWD")"
  echo "BASE_URL=$BASE_URL"
  echo "CI=$CI"
  echo "password-set=$([ -n "${V_NOTE_SMOKE_PASSWORD:-}" ] && echo yes || echo no)"
} >> "$STUB_LOG"
exit "${STUB_NPX_EXIT:-0}"
STUB
chmod +x "$WORK/bin/npm" "$WORK/bin/npx"

python3 - "$PORT" <<'PY' &
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200 if self.path == "/health" else 404)
        self.end_headers()

    def log_message(self, *_):
        pass


HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
PY
STUB_PID=$!

for _ in $(seq 1 50); do
  curl -fsS -o /dev/null "http://127.0.0.1:$PORT/health" 2>/dev/null && break
  sleep 0.1
done

check() {
  if [ "$1" = 0 ]; then
    echo "  ok   — $2"
  else
    echo "  FAIL — $2"
    failures=$((failures + 1))
  fi
}

# run <mode> [env overrides…] — runs the script with a complete, healthy
# environment, then applies the overrides (`VAR=` unsets by emptying).
run() {
  local mode=$1
  shift
  : > "$WORK/log"
  set +e
  env PATH="$WORK/bin:$PATH" STUB_LOG="$WORK/log" \
    V_NOTE_HOST="127.0.0.1:$PORT" SMOKE_SCHEME=http SMOKE_ATTEMPTS=2 SMOKE_DELAY=0 \
    V_NOTE_SMOKE_USERNAME=v-note-smoke-dev V_NOTE_SMOKE_EMAIL=smoke@smoke.invalid \
    V_NOTE_SMOKE_PASSWORD="$PASSWORD" \
    "$@" ./scripts/smoke-web-live.sh > "$WORK/out" 2>&1
  status=$?
  set -e
  if grep -qF "$PASSWORD" "$WORK/out" "$WORK/log"; then
    check 1 "$mode: the password never appears in the output"
  else
    check 0 "$mode: the password never appears in the output"
  fi
}

ran_spec() { grep -q '^npx ' "$WORK/log"; }

echo "==> missing inputs fail before anything runs"
for var in V_NOTE_HOST V_NOTE_SMOKE_USERNAME V_NOTE_SMOKE_EMAIL V_NOTE_SMOKE_PASSWORD; do
  run "missing-$var" "$var="
  [ "$status" != 0 ] && ! ran_spec
  check $? "missing-$var: fails without running the spec"
  grep -q "$var must be set" "$WORK/out"
  check $? "missing-$var: names the variable"
done

echo "==> an app that never becomes healthy fails before the spec"
run app-down V_NOTE_HOST="127.0.0.1:1"
[ "$status" != 0 ] && ! ran_spec
check $? "app-down: fails without running the spec"

echo "==> a healthy app runs the live config against the host"
run healthy
check "$status" "healthy: passes"
grep -qx 'npm ci --no-audit --no-fund' "$WORK/log"
check $? "healthy: installs the pinned e2e dependencies with npm ci"
grep -qx 'npx playwright test --config playwright.live.config.ts' "$WORK/log"
check $? "healthy: runs the live config, not the mock-IdP suite"
grep -qx 'cwd=e2e' "$WORK/log"
check $? "healthy: runs from e2e/"
grep -qx "BASE_URL=http://127.0.0.1:$PORT" "$WORK/log"
check $? "healthy: BASE_URL is the deployed host"
grep -qx 'CI=1' "$WORK/log"
check $? "healthy: CI=1 (one retry, list reporter)"
grep -qx 'password-set=yes' "$WORK/log"
check $? "healthy: the spec receives the password"

echo "==> a failing spec fails the step"
run spec-fails STUB_NPX_EXIT=1
[ "$status" != 0 ]
check $? "spec-fails: exits non-zero"

if [ "$failures" != 0 ]; then
  echo
  echo "smoke-web-live.sh tests FAILED ($failures check(s))"
  exit 1
fi
echo
echo "smoke-web-live.sh tests OK"
