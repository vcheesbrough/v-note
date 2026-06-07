#!/usr/bin/env bash
# Seed or rotate v-note compose secrets in OpenBao (local compose + operator bootstrap).
#
# KV path: secret/v-note-stack/env
# Woodpecker deploy uses separate keys under secret/woodpecker/repos/vcheesbrough/v-note
# (v_note_dev_postgres_password, v_note_dev_oidc_client_secret,
#  v_note_dev_assetlinks_json, v_note_prod_assetlinks_json, etc.).
# App Links JSON is Woodpecker-only — see docs/DEPLOY.md for jq -c seeding.
#
# Usage:
#   export BAO_ADDR=https://secrets.desync.link
#   export BAO_TOKEN=<token with write to secret/v-note-stack/*>
#   export POSTGRES_PASSWORD=$(openssl rand -base64 24)
#   export OIDC_CLIENT_SECRET=test-secret   # mock OIDC default; use real Authentik secret when needed
#   ./scripts/patch-v-note-openbao-secrets.sh
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
: "${OIDC_CLIENT_SECRET:?OIDC_CLIENT_SECRET is required}"

KV_PATH="secret/v-note-stack/env"
export BAO_ADDR

ARGS=(
  POSTGRES_PASSWORD="$POSTGRES_PASSWORD"
  OIDC_CLIENT_SECRET="$OIDC_CLIENT_SECRET"
)

if bao kv get "$KV_PATH" >/dev/null 2>&1; then
  echo "==> Patching $KV_PATH"
  bao kv patch "$KV_PATH" "${ARGS[@]}"
else
  echo "==> Creating $KV_PATH"
  bao kv put "$KV_PATH" "${ARGS[@]}"
fi

echo "==> Done. Run ./scripts/fetch-compose-env.sh then just run-compose."
