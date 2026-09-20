#!/usr/bin/env bash
# Test scripts/smoke-sql-console.sh against a local stub, with no Traefik,
# no Authentik and no deployment.
#
# Why this exists: that smoke check is the ONLY automated assertion that the SQL
# console is not publicly readable — no CI stack has Traefik or Authentik in it,
# so nothing else can make the claim. A guard that silently passes is worse than
# no guard, because the card's acceptance criterion says the exposure check is
# automated rather than done by hand.
#
# So this drives it against a stub that impersonates the deployed host and
# asserts it reaches the right verdict in each case:
#
#   gated        → /dbconsole/ redirects to the IdP                  → passes
#   exposed      → /dbconsole/ serves 200 to an anonymous request    → FAILS
#   unattached   → /dbconsole/ 404s (provider not on the outpost)    → fails
#   backstop     → /dbconsole/ 401s from pgweb, no forward-auth      → fails
#   wrong-idp    → redirects somewhere that is not the IdP           → fails
#   no-callback  → the outpost path hits the SPA catch-all (200)     → fails
#   app-broken   → gating the console broke the SPA on the same host → fails
#
# Mirrors scripts/test-smoke-oidc-login.sh exactly.

set -euo pipefail

cd "$(dirname "$0")/.."

PORT="${TEST_PORT:-18398}"
WORK=$(mktemp -d)
STUB_PID=""

cleanup() {
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo gated > "$WORK/mode"

python3 - "$PORT" "$WORK" <<'STUB' &
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
WORK = sys.argv[2]
IDP = "http://stub-idp.invalid/application/o/authorize/"


def mode():
    with open(f"{WORK}/mode") as handle:
        return handle.read().strip()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _status(self, code, location=None):
        self.send_response(code)
        if location:
            self.send_header("Location", location)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        current = mode()

        if self.path.startswith("/dbconsole"):
            if current == "exposed":
                return self._status(200)
            if current == "unattached":
                return self._status(404)
            if current == "backstop":
                return self._status(401)
            if current == "wrong-idp":
                return self._status(302, "http://example.invalid/somewhere")
            return self._status(302, IDP)

        if self.path.startswith("/outpost.goauthentik.io"):
            # 200 is the SPA catch-all answering — the missing-callback-router bug.
            return self._status(200 if current == "no-callback" else 302, IDP)

        if self.path == "/":
            return self._status(503 if current == "app-broken" else 200)

        return self._status(404)


HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
STUB
STUB_PID=$!

for _ in $(seq 1 50); do
  curl -sS -o /dev/null "http://127.0.0.1:${PORT}/" 2>/dev/null && break
  sleep 0.1
done

failures=0

check() {
  if [ "$1" = "ok" ]; then
    echo "  ok   — $2"
  else
    echo "  FAIL — $2"
    failures=$((failures + 1))
  fi
}

run_mode() {
  echo "$1" > "$WORK/mode"
  set +e
  V_NOTE_HOST="127.0.0.1:${PORT}" \
  SMOKE_SCHEME=http \
  SMOKE_IDP_HOST=stub-idp.invalid \
  SMOKE_ATTEMPTS=1 \
  SMOKE_DELAY=0 \
    ./scripts/smoke-sql-console.sh >"$WORK/out" 2>&1
  RC=$?
  set -e
}

expect_pass() {
  run_mode "$1"
  check "$([ "$RC" -eq 0 ] && echo ok)" "$1 → passes (rc=$RC)"
}

expect_fail() {
  run_mode "$1"
  check "$([ "$RC" -ne 0 ] && echo ok)" "$1 → fails (rc=$RC)"
  if [ -n "${2:-}" ]; then
    check "$(grep -qi -- "$2" "$WORK/out" && echo ok)" "$1 → explains it ('$2')"
  fi
}

echo "==> a properly gated console passes"
expect_pass gated

echo "==> the failure this check exists to catch"
expect_fail exposed "ANONYMOUS"

echo "==> the other ways it can be wrong"
expect_fail unattached "not attached to the outpost"
expect_fail backstop "no identity gate"
expect_fail wrong-idp "redirected somewhere other than"
expect_fail no-callback "callback router is missing"
expect_fail app-broken "SPA no longer serves"

if [ "$failures" -gt 0 ]; then
  echo
  echo "smoke-sql-console validation FAILED ($failures check(s))"
  exit 1
fi
echo
echo "smoke-sql-console OK"
