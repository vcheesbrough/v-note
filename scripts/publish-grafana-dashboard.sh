#!/bin/sh
# Publish a Grafana dashboard from a JSON file over the Grafana HTTP API.
#
#   publish-grafana-dashboard.sh [--dry-run] <dashboard.json>
#
# Repo-agnostic on purpose. The folder, the file and the version message are all
# inputs, so another application repo can copy this unchanged.
#
# Environment:
#   GRAFANA_FOLDER_UID      folder to publish into (v-note: `v-note`, under Applications)
#   RELEASE_TAG             release being deployed — goes into the version message
#   COMMIT_SHA              commit being deployed — goes into the version message
#   GRAFANA_MESSAGE_PREFIX  optional version-message prefix; defaults to GRAFANA_FOLDER_UID
#   GRAFANA_URL             e.g. https://grafana.desync.link  (not read by --dry-run)
#   GRAFANA_TOKEN           token with Edit on the folder     (not read by --dry-run)
#
# The request is `POST /api/dashboards/db` with
#   { dashboard: <file with id: null>, folderUid, overwrite: true, message }
# `id: null` plus the file's stable `uid` updates the same dashboard in place on
# every publish, and `message` (`<prefix> <release> <sha>`) ties each entry in
# Grafana's version history to the commit that produced it. Anything published
# this way is overwritten by the next publish: edits made in the Grafana UI are
# lost unless exported back into the repo.
#
# --dry-run prints that request body and exits without touching the network,
# which is what scripts/test-grafana-dashboard.sh asserts on.
#
# The token never appears in argv or in the output. curl reads the Authorization
# header from stdin (`--header @-`, fed by the `printf` builtin), and a failure
# prints only the HTTP status and Grafana's response body.
#
# POSIX sh + jq (+ curl when publishing): CI runs this on alpine.
set -eu

usage() {
  echo "usage: $0 [--dry-run] <dashboard.json>" >&2
  exit 2
}

DRY_RUN=false
if [ "${1:-}" = "--dry-run" ]; then
  DRY_RUN=true
  shift
fi
[ $# -eq 1 ] || usage
DASHBOARD_FILE="$1"

# `require NAME` — fail when the variable is unset or blank. Names are literals
# from this file, and the value is never printed.
require() {
  eval "required_value=\${$1:-}"
  if [ -z "$required_value" ]; then
    echo "ERROR: $1 is required" >&2
    exit 1
  fi
}

require GRAFANA_FOLDER_UID
require RELEASE_TAG
require COMMIT_SHA
if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq is required" >&2
  exit 1
fi
if [ ! -f "$DASHBOARD_FILE" ]; then
  echo "ERROR: dashboard file not found: $DASHBOARD_FILE" >&2
  exit 1
fi
# A stable uid is what makes `overwrite: true` update one dashboard; without it
# Grafana would create a new dashboard on every publish.
if ! jq -e '(.uid | type) == "string" and (.uid | length) > 0' "$DASHBOARD_FILE" >/dev/null 2>&1; then
  echo "ERROR: $DASHBOARD_FILE is not dashboard JSON with a non-empty uid" >&2
  exit 1
fi

MESSAGE="${GRAFANA_MESSAGE_PREFIX:-$GRAFANA_FOLDER_UID} $RELEASE_TAG $COMMIT_SHA"

render_payload() {
  jq --arg folderUid "$GRAFANA_FOLDER_UID" --arg message "$MESSAGE" \
    '{dashboard: (. + {id: null}), folderUid: $folderUid, overwrite: true, message: $message}' \
    "$DASHBOARD_FILE"
}

if [ "$DRY_RUN" = true ]; then
  render_payload
  exit 0
fi

require GRAFANA_URL
require GRAFANA_TOKEN
if ! command -v curl >/dev/null 2>&1; then
  echo "ERROR: curl is required" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
render_payload > "$WORK/payload.json"

URL="${GRAFANA_URL%/}/api/dashboards/db"
if ! status=$(printf 'Authorization: Bearer %s\n' "$GRAFANA_TOKEN" | curl --silent --show-error \
  --max-time 60 --request POST \
  --header @- --header 'Content-Type: application/json' \
  --data-binary "@$WORK/payload.json" \
  --output "$WORK/response" --write-out '%{http_code}' \
  "$URL"); then
  echo "ERROR: POST $URL failed before Grafana answered" >&2
  exit 1
fi

case "$status" in
  2[0-9][0-9]) ;;
  *)
    echo "ERROR: Grafana answered HTTP $status to POST $URL for $DASHBOARD_FILE:" >&2
    cat "$WORK/response" >&2 2>/dev/null || true
    echo >&2
    exit 1
    ;;
esac

jq -r '"published \(.uid // "?") as version \(.version // "?"): \(.url // "?")"' "$WORK/response" 2>/dev/null \
  || cat "$WORK/response"
echo "version message: $MESSAGE"
