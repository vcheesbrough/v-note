#!/usr/bin/env bash
# Render minified Android App Links JSON for ASSETLINKS_JSON (stdout).
# Usage: ./scripts/render-assetlinks-json.sh <dev|prod> <sha256_colon_fingerprint>
set -euo pipefail

ENV="${1:?usage: render-assetlinks-json.sh <dev|prod> <sha256_fingerprint>}"
SHA="${2:?sha256 fingerprint required (colon-separated, from keytool or signingReport)}"

case "$ENV" in
  dev)
    PACKAGE="link.desync.vnote.dev"
    ;;
  prod)
    PACKAGE="link.desync.vnote"
    ;;
  *)
    echo "ERROR: env must be dev or prod (got: $ENV)" >&2
    exit 1
    ;;
esac

jq -nc \
  --arg pkg "$PACKAGE" \
  --arg sha "$SHA" \
  '[{
    relation: ["delegate_permission/common.handle_all_urls"],
    target: {
      namespace: "android_app",
      package_name: $pkg,
      sha256_cert_fingerprints: [$sha]
    }
  }]'
