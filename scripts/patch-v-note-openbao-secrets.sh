#!/usr/bin/env bash
# Seed or rotate v-note compose secrets in OpenBao (local compose + operator bootstrap).
#
# KV path: secret/v-note-stack/env
# Woodpecker deploy uses separate keys under secret/woodpecker/repos/vcheesbrough/v-note
# (v_note_dev_postgres_password, v_note_dev_sovereign_access_url — one pair per
# deployed environment, and dev is the only one) — patch those with
# `bao kv patch`, and see
# scripts/store-sovereign-access-url.sh for the access URLs. App Links JSON now lives
# in sovereign-config (android/assetlinks-json), not OpenBao — see docs/DEPLOY.md.
#
# Usage:
#   export BAO_ADDR=https://secrets.desync.link
#   export BAO_TOKEN=<token with write to secret/v-note-stack/*>
#   export POSTGRES_PASSWORD=$(openssl rand -base64 24)
#   ./scripts/patch-v-note-openbao-secrets.sh
#
# There is no OIDC client secret: since #274 the SPA and Android share one public
# Authentik client using Authorization Code + PKCE.
#
# Idempotent: uses `bao kv patch` when the path exists, otherwise `bao kv put`.

set -euo pipefail

command -v bao >/dev/null || {
  echo "ERROR: bao CLI not found (install OpenBao/Vault client)."
  exit 1
}

: "${BAO_ADDR:?BAO_ADDR is required}"
: "${BAO_TOKEN:?BAO_TOKEN is required}"
: "${POSTGRES_PASSWORD:?POSTGRES_PASSWORD is required}"

KV_PATH="secret/v-note-stack/env"
export BAO_ADDR

ARGS=(
  POSTGRES_PASSWORD="$POSTGRES_PASSWORD"
)

if bao kv get "$KV_PATH" >/dev/null 2>&1; then
  echo "==> Patching $KV_PATH"
  bao kv patch "$KV_PATH" "${ARGS[@]}"
else
  echo "==> Creating $KV_PATH"
  bao kv put "$KV_PATH" "${ARGS[@]}"
fi

echo "==> Done. Run ./scripts/fetch-compose-env.sh then just run-compose."
