#!/usr/bin/env bash
# Post-deploy smoke, authenticated half (#179): can a real user sign in to the
# deployed app through the real IdP and use it?
#
# smoke-oidc-login.sh stops at Authentik's login page on purpose. Everything
# past that page — the password stage, the code exchange, the session cookie,
# the API behind it, the page channel, Postgres — was only ever exercised
# against the mock IdP in e2e/, before the deploy. This runs the live spec in
# e2e/live/ against the deployed host, signing in as the blueprint's smoke user
# (authentik/blueprint-dev.yaml): sign in → /api/me → create a page → commit a
# stroke → both survive a reload → delete the page → sign out.
#
# Runs in the Playwright image the e2e stack pins (.woodpecker/deploy.yml), from
# the repo root. The spec, not this script, owns every product assertion; this
# only checks its inputs, waits for the app, and runs it.
#
# The password arrives through the environment (a masked Woodpecker secret) and
# is never printed: no `set -x`, and nothing below echoes the environment.
#
# Usage (see docs/DEPLOY.md → Post-deploy smoke for running it by hand):
#   V_NOTE_HOST=v-notes-dev.desync.link \
#   V_NOTE_SMOKE_USERNAME=v-note-smoke-dev \
#   V_NOTE_SMOKE_EMAIL=v-note-smoke-dev@smoke.invalid \
#   V_NOTE_SMOKE_PASSWORD=… ./scripts/smoke-web-live.sh

set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
  echo "FAIL — $1" >&2
  exit 1
}

HOST="${V_NOTE_HOST:-}"
[ -n "$HOST" ] || fail "V_NOTE_HOST must be set (e.g. v-notes-dev.desync.link)"
[ -n "${V_NOTE_SMOKE_USERNAME:-}" ] || fail "V_NOTE_SMOKE_USERNAME must be set"
[ -n "${V_NOTE_SMOKE_EMAIL:-}" ] || fail "V_NOTE_SMOKE_EMAIL must be set"
# Named but never shown. An empty value is how a missing broker leaf would look
# if Woodpecker ever stopped refusing it, and signing in with it would fail as a
# confusing "wrong password" deep inside the spec.
[ -n "${V_NOTE_SMOKE_PASSWORD:-}" ] || fail "V_NOTE_SMOKE_PASSWORD must be set (Woodpecker secret v_note_dev_smoke_password)"

ATTEMPTS="${SMOKE_ATTEMPTS:-10}"
DELAY="${SMOKE_DELAY:-6}"
# Always https in the pipeline; scripts/test-smoke-web-live.sh points it at a
# local stub.
SCHEME="${SMOKE_SCHEME:-https}"
BASE_URL="$SCHEME://$HOST"

# auto-deploy-dev already gated on the container's health, but Traefik can take
# a moment to route to a recreated container. Same budget as smoke-oidc-login.
ready=""
for attempt in $(seq 1 "$ATTEMPTS"); do
  if curl -fsS -o /dev/null --max-time 10 "$BASE_URL/health"; then
    ready=1
    break
  fi
  echo "waiting for $BASE_URL/health (attempt $attempt/$ATTEMPTS)"
  sleep "$DELAY"
done
[ -n "$ready" ] || fail "$BASE_URL/health never answered 2xx"

cd e2e
npm ci --no-audit --no-fund
# CI=1 selects the CI behaviour of the live config (one retry, list reporter)
# whether or not the runner sets it.
CI=1 BASE_URL="$BASE_URL" npx playwright test --config playwright.live.config.ts
