#!/usr/bin/env bash
# Seed or rotate Woodpecker deploy secrets in OpenBao (mini broker path).
#
# KV path: secret/woodpecker/repos/vcheesbrough/v-note
# Keys match .woodpecker/build.yml from_secret names.
#
# Usage (on mini or with BAO_TOKEN that can write the path):
#   export BAO_ADDR=https://secrets.desync.link
#   export BAO_TOKEN=<token>
#
#   # Dev App Links — local debug keystore (fast):
#   export V_NOTE_DEV_ANDROID_CERT_SHA256="$(./scripts/android-dev-debug-fingerprint.sh)"
#   ./scripts/patch-v-note-woodpecker-openbao-secrets.sh
#
#   # Or set fingerprints explicitly (prod release keystore, etc.):
#   export V_NOTE_DEV_ANDROID_CERT_SHA256='AA:BB:...'
#   export V_NOTE_PROD_ANDROID_CERT_SHA256='11:22:...'
#   ./scripts/patch-v-note-woodpecker-openbao-secrets.sh
#
# Idempotent: patches only keys for env vars you set.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

command -v bao >/dev/null || {
  echo "ERROR: bao CLI not found (install OpenBao/Vault client)."
  exit 1
}

: "${BAO_ADDR:?BAO_ADDR is required}"
: "${BAO_TOKEN:?BAO_TOKEN is required}"

KV_PATH="secret/woodpecker/repos/vcheesbrough/v-note"
export BAO_ADDR

ARGS=()

if [ -n "${V_NOTE_DEV_ANDROID_CERT_SHA256:-}" ]; then
  ARGS+=(
    v_note_dev_assetlinks_json="$("$ROOT/scripts/render-assetlinks-json.sh" dev "$V_NOTE_DEV_ANDROID_CERT_SHA256")"
  )
fi

if [ -n "${V_NOTE_PROD_ANDROID_CERT_SHA256:-}" ]; then
  ARGS+=(
    v_note_prod_assetlinks_json="$("$ROOT/scripts/render-assetlinks-json.sh" prod "$V_NOTE_PROD_ANDROID_CERT_SHA256")"
  )
fi

if [ "${#ARGS[@]}" -eq 0 ]; then
  echo "ERROR: set V_NOTE_DEV_ANDROID_CERT_SHA256 and/or V_NOTE_PROD_ANDROID_CERT_SHA256" >&2
  echo "  Dev auto: export V_NOTE_DEV_ANDROID_CERT_SHA256=\"\$(./scripts/android-dev-debug-fingerprint.sh)\"" >&2
  echo "  (CI keystore, slow: add --docker to fingerprint script)" >&2
  exit 1
fi

if bao kv get "$KV_PATH" >/dev/null 2>&1; then
  echo "==> Patching $KV_PATH (${#ARGS[@]} key(s))"
  bao kv patch "$KV_PATH" "${ARGS[@]}"
else
  echo "==> Creating $KV_PATH (${#ARGS[@]} key(s))"
  bao kv put "$KV_PATH" "${ARGS[@]}"
fi

echo "==> Done. Redeploy dev/prod from Woodpecker when ready."
