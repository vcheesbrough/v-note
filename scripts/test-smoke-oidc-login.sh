#!/usr/bin/env bash
# Test scripts/smoke-oidc-login.sh against a local stub, with no Authentik and no
# deployment.
#
# Why this exists: smoke-oidc-login.sh is the gate that #372 was missing, and its
# entire value is that it *fails* on a rejected authorize request. A guard that
# silently passes is the same class of bug it was written to catch — and it only
# ever runs post-deploy against live infrastructure, where a broken assertion
# would be discovered by the next outage rather than by CI.
#
# So this drives the smoke check against stubs that impersonate both the app's
# /auth/login and the IdP's authorize endpoint, and asserts it reaches the right
# verdict in each case:
#
#   healthy      → IdP redirects to its login flow                    → passes
#   rejected     → IdP redirects to the app callback with `error=`    → fails (#372)
#   bounced      → IdP redirects to the callback with no `error=`     → fails
#   unknown      → IdP redirects somewhere unrecognised               → fails
#   no-pkce      → app omits the PKCE challenge                       → fails
#   not-idp      → app redirects somewhere other than the IdP         → fails
#   app-down     → app never redirects at all                         → fails
#
# Each mode gets its own stub port, so the check under test is driven with exactly
# the URL shape it uses in production — no test-only parameters threaded through
# it.
#
# Mirrors scripts/test-deploy-v-note.sh and scripts/test-grafana-dashboard.sh: a
# repo-owned script CI points at shared infrastructure gets a test that runs
# without that infrastructure.

set -euo pipefail

cd "$(dirname "$0")/.."

PORT="${TEST_PORT:-18372}"
MODES=(healthy rejected bounced unknown no-pkce not-idp app-down)
STUB_PID=""

cleanup() {
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null || true
}
trap cleanup EXIT

python3 - "$PORT" "${MODES[@]}" <<'STUB' &
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse

BASE_PORT = int(sys.argv[1])
MODES = sys.argv[2:]


def idp_responses(base):
    return {
        "healthy": (
            f"{base}/if/flow/default-authentication-flow/"
            "?next=%2Fapplication%2Fo%2Fauthorize%2F"
        ),
        "rejected": (
            f"{base}/auth/callback?error=invalid_request"
            "&error_description=The%20request%20is%20otherwise%20malformed&state=smoke"
        ),
        "bounced": f"{base}/auth/callback?code=abc&state=smoke",
        "unknown": f"{base}/somewhere/else/",
    }


def make_handler(mode, base):
    class Stub(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            path = urlparse(self.path).path

            # The app's own /auth/login.
            if path == "/auth/login":
                if mode == "app-down":
                    self.send_response(503)
                    self.end_headers()
                    return
                if mode == "not-idp":
                    target = f"{base}/not-the-idp/"
                else:
                    params = (
                        "response_type=code&client_id=v-note-test"
                        "&redirect_uri=http%3A%2F%2F127.0.0.1%2Fauth%2Fcallback"
                        "&state=smoke"
                    )
                    if mode != "no-pkce":
                        params += "&code_challenge=abc123&code_challenge_method=S256"
                    target = f"{base}/application/o/authorize/?{params}"
                self.send_response(303)
                self.send_header("Location", target)
                self.end_headers()
                return

            # The IdP's authorize endpoint.
            if path == "/application/o/authorize/":
                responses = idp_responses(base)
                self.send_response(302)
                self.send_header("Location", responses.get(mode, responses["healthy"]))
                self.end_headers()
                return

            self.send_response(404)
            self.end_headers()

    return Stub


for offset, mode in enumerate(MODES):
    port = BASE_PORT + offset
    base = f"http://127.0.0.1:{port}"
    server = HTTPServer(("127.0.0.1", port), make_handler(mode, base))
    threading.Thread(target=server.serve_forever, daemon=True).start()

threading.Event().wait()
STUB
STUB_PID=$!

port_for() {
  local target="$1" offset=0 mode
  for mode in "${MODES[@]}"; do
    if [ "$mode" = "$target" ]; then
      echo $((PORT + offset))
      return
    fi
    offset=$((offset + 1))
  done
  echo "unknown stub mode: $target" >&2
  exit 1
}

# Wait for every stub port to accept connections rather than sleeping a fixed
# interval.
for mode in "${MODES[@]}"; do
  ready=""
  for _ in $(seq 1 100); do
    if curl -sS -o /dev/null "http://127.0.0.1:$(port_for "$mode")/auth/login" 2>/dev/null; then
      ready=1
      break
    fi
    sleep 0.1
  done
  [ -n "$ready" ] || { echo "stub for mode '$mode' never came up" >&2; exit 1; }
done

failures=0

run_case() {
  local mode="$1" expected="$2" description="$3"
  local output status

  set +e
  output=$(
    SMOKE_SCHEME=http \
    V_NOTE_HOST="127.0.0.1:$(port_for "$mode")" \
    SMOKE_ATTEMPTS=2 SMOKE_DELAY=0 \
    ./scripts/smoke-oidc-login.sh 2>&1
  )
  status=$?
  set -e

  if { [ "$expected" = "pass" ] && [ "$status" -eq 0 ]; } ||
     { [ "$expected" = "fail" ] && [ "$status" -ne 0 ]; }; then
    echo "  ok   — $description"
  else
    echo "  FAIL — $description (expected to $expected, exit $status)"
    echo "$output" | sed 's/^/           /'
    failures=$((failures + 1))
  fi
}

echo "==> the smoke check reaches the right verdict"
run_case healthy  pass "a healthy IdP login flow passes"
run_case rejected fail "an \`error=\` redirect back to the callback fails (the #372 outage)"
run_case bounced  fail "a redirect straight back to the callback fails even without \`error=\`"
run_case unknown  fail "an unrecognised IdP response fails rather than passing silently"
run_case no-pkce  fail "an authorize request without a PKCE challenge fails"
run_case not-idp  fail "a redirect somewhere other than the IdP fails"
run_case app-down fail "an app that never redirects fails"

if [ "$failures" -ne 0 ]; then
  echo
  echo "smoke-oidc-login validation FAILED ($failures case(s))"
  exit 1
fi

echo
echo "smoke-oidc-login OK"
