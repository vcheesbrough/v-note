#!/usr/bin/env bash
# Render minified Android App Links JSON for ASSETLINKS_JSON (stdout).
# Usage: ./scripts/render-assetlinks-json.sh <dev> <sha256_colon_fingerprint>
#
# The <env> positional stays even with one case arm: each environment has its own
# application id and its own signing certificate, so #388 adds an arm rather than
# reworking the script.
set -euo pipefail

ENV="${1:?usage: render-assetlinks-json.sh <dev> <sha256_fingerprint>}"
SHA="${2:?sha256 fingerprint required (colon-separated, from keytool or signingReport)}"

case "$ENV" in
  dev)
    PACKAGE="link.desync.vnote.dev"
    ;;
  *)
    echo "ERROR: env must be dev (got: $ENV) — dev is the only environment; see #388" >&2
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
