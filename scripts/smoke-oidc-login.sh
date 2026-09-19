#!/usr/bin/env bash
# Post-deploy smoke check: can a user actually start a login?
#
# Why this exists: #372 took login down in both environments and every automated
# gate stayed green. The blueprint applied cleanly, the app was healthy, and the
# e2e suite passed — because the e2e harness runs `navikt/mock-oauth2-server`,
# which has no client registry at all. It auto-approves any authorize request, so
# it cannot see a provider that the real Authentik would reject. The bug was a
# missing `grant_types` on the provider, and the only place it was visible was
# the live IdP's answer to a real authorize request.
#
# So this drives the one hop the mock can never model: app → real IdP.
#
#   1. GET https://$V_NOTE_HOST/auth/login  → expect a redirect to the IdP's
#      authorize endpoint (proves the app resolved its OIDC config and built a
#      request).
#   2. GET that authorize URL              → expect the IdP to hand back its
#      login flow, NOT an `error=` redirect to our callback.
#
# Step 2 is the assertion that matters. An unauthenticated authorize against a
# healthy provider redirects to `/if/flow/<authentication-flow>/`; a provider the
# IdP rejects redirects to the app's own callback carrying `?error=...`, which the
# server then renders as the bare `authentication denied: ...` page users saw.
#
# No credentials and no session: this deliberately stops at the login page rather
# than authenticating, so it needs no test user and is safe to run against prod.
#
# Usage: V_NOTE_HOST=v-notes-dev.desync.link ./scripts/smoke-oidc-login.sh

set -euo pipefail

HOST="${V_NOTE_HOST:?V_NOTE_HOST must be set (e.g. v-notes-dev.desync.link)}"
ATTEMPTS="${SMOKE_ATTEMPTS:-10}"
DELAY="${SMOKE_DELAY:-6}"
# Always https in the pipeline; scripts/test-smoke-oidc-login.sh overrides it to
# point the check at a local stub IdP so its failure modes can be tested.
SCHEME="${SMOKE_SCHEME:-https}"

fail() {
  echo "FAIL — $1" >&2
  exit 1
}

# The app does OIDC discovery at startup with its own retries, and the container
# has just been recreated, so /auth/login can 5xx briefly before it settles.
#
# One request per attempt, reading the status and the Location together: asking
# twice starts two flows (each mints its own state and PKCE verifier) and lets
# the two answers disagree, so the URL asserted below would not be the one whose
# status was checked. `|| true` keeps a refused connection from killing the loop
# under `set -e` — that is the case the retry exists for.
authorize_url=""
status=000
for attempt in $(seq 1 "$ATTEMPTS"); do
  probe=$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' --max-time 15 \
    "$SCHEME://$HOST/auth/login" 2>/dev/null || true)
  status="${probe%% *}"
  [ -n "$status" ] || status=000

  case "$status" in
    302 | 303)
      authorize_url="${probe#* }"
      break
      ;;
  esac

  echo "  attempt $attempt/$ATTEMPTS: /auth/login returned $status, retrying in ${DELAY}s"
  sleep "$DELAY"
done

[ -n "$authorize_url" ] || fail "$SCHEME://$HOST/auth/login never redirected to the IdP (last status $status)"

echo "==> /auth/login redirects to the IdP"
echo "    $authorize_url"

case "$authorize_url" in
  *"/application/o/authorize/"*) ;;
  *) fail "/auth/login redirected somewhere other than the IdP authorize endpoint: $authorize_url" ;;
esac

for param in "response_type=code" "client_id=" "code_challenge=" "code_challenge_method=S256"; do
  case "$authorize_url" in
    *"$param"*) ;;
    *) fail "authorize URL is missing '$param' — $authorize_url" ;;
  esac
done
echo "  ok   — authorize request carries response_type=code and a PKCE S256 challenge"

# The hop the mock IdP cannot model: what does the real Authentik say?
location=$(curl -sS -o /dev/null -w '%{redirect_url}' --max-time 15 "$authorize_url")
[ -n "$location" ] || fail "the IdP did not redirect in response to the authorize request"

echo "==> the IdP answered the authorize request"
echo "    $location"

case "$location" in
  *"error="*)
    # Surface the IdP's own words — this is what the outage looked like.
    fail "the IdP rejected the authorize request: $location"
    ;;
esac

# A rejection redirects back to our own callback; an acceptance goes to the IdP's
# login flow. Checking the destination as well as the absence of `error=` stops a
# silent pass if Authentik ever changes how it reports the failure.
case "$location" in
  *"/auth/callback"*)
    fail "the IdP bounced straight back to the app callback instead of presenting a login: $location"
    ;;
  *"/if/flow/"*)
    echo "  ok   — the IdP presented its login flow"
    ;;
  *)
    fail "unexpected IdP response, neither a login flow nor a known rejection: $location"
    ;;
esac

echo
echo "OIDC login smoke check OK for $SCHEME://$HOST"
