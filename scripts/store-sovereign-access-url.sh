#!/usr/bin/env bash
# Store a sovereign-config access URL as the Woodpecker deploy secret for one env.
#
# KV path: secret/woodpecker/repos/vcheesbrough/v-note
# Keys:    v_note_dev_sovereign_access_url / v_note_prod_sovereign_access_url
#          (match the from_secret names in .woodpecker/deploy.yml)
#
# The access URL is a long-lived credential granting read access to the whole
# /v-note/<env>/server subtree — including database/password and
# oidc/client-secret. It is therefore read from STDIN and piped onward to bao via
# its `key=-` stdin form, never as an argument — so it lands in neither this
# script's argv, nor bao's (/proc/<pid>/cmdline), nor shell history.
#
# Get a URL by creating a managed connection (sovereign-config MCP or web UI):
#   create_connection root=/v-note/dev/server  permissions=["read"]
#   create_connection root=/v-note/prod/server permissions=["read"]
# The URL is shown once at creation — pipe it straight into this script.
#
# Usage:
#   export BAO_ADDR=https://secrets.desync.link
#   export BAO_TOKEN=<token that can write the KV path>
#   ./scripts/store-sovereign-access-url.sh dev     # then paste the URL + Ctrl-D
#   pbpaste | ./scripts/store-sovereign-access-url.sh prod   # or pipe it in
#
# Idempotent: patches only the one key for the given env.

set -euo pipefail

usage() {
  echo "Usage: $0 dev|prod   (access URL on stdin)" >&2
}

target="${1:-}"
if [ "$target" != "dev" ] && [ "$target" != "prod" ]; then
  usage
  exit 1
fi

command -v bao >/dev/null || {
  echo "ERROR: bao CLI not found (install OpenBao/Vault client)." >&2
  exit 1
}

: "${BAO_ADDR:?BAO_ADDR is required}"
: "${BAO_TOKEN:?BAO_TOKEN is required}"
export BAO_ADDR BAO_TOKEN

KV_PATH="secret/woodpecker/repos/vcheesbrough/v-note"
KEY="v_note_${target}_sovereign_access_url"

if [ -t 0 ]; then
  echo "Paste the $target access URL, then press Ctrl-D:" >&2
fi
url="$(cat)"
url="${url#"${url%%[![:space:]]*}"}"
url="${url%"${url##*[![:space:]]}"}"

if [ -z "$url" ]; then
  echo "ERROR: no access URL on stdin" >&2
  exit 1
fi

# Sanity-check shape without echoing the credential.
case "$url" in
  http://*|https://*) ;;
  *) echo "ERROR: access URL should be an http(s) URL (got ${#url} chars)" >&2; exit 1 ;;
esac

echo "==> Storing $KEY (${#url} chars) at $KV_PATH"
# `KEY=-` makes bao read the value from stdin, so only the key *name* reaches argv.
# Passing "$KEY=$url" would expose the credential in /proc/<pid>/cmdline for the
# duration of the call. `printf '%s'` adds no trailing newline, so the stored value
# is byte-exact regardless of whether bao trims one.
# Always `patch`, never `put`. KV-v2 `put` REPLACES the whole data map, and this
# path is shared with five other secrets (both postgres passwords and both OIDC
# client secrets), so a `put` here would delete them. Deciding between the two by
# probing with `kv get` is not safe either: a write-only token — which is exactly
# what the usage above asks for — cannot read, so the probe fails and the
# destructive branch is taken by an operator following the runbook correctly.
#
# `patch` merges and cannot delete siblings. If the path does not exist yet, patch
# fails and we say so, leaving creation as a deliberate one-time act.
if ! printf '%s' "$url" | bao kv patch "$KV_PATH" "$KEY=-" >/dev/null; then
  echo "ERROR: failed to patch $KEY at $KV_PATH" >&2
  echo "  If the path does not exist yet, create it once with an explicit" >&2
  echo "  \`bao kv put $KV_PATH <key>=<value>\` — this script never uses put," >&2
  echo "  because put would replace the other secrets stored at that path." >&2
  exit 1
fi

echo "==> Done. Redeploy $target from Woodpecker when both envs are stored."
