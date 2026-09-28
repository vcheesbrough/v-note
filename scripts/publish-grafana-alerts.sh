#!/bin/sh
# Publish a Grafana-managed alert rule group from a JSON file (#439).
#
#   publish-grafana-alerts.sh [--dry-run] <rule-group.json>
#
# The dashboard's counterpart for alerts (publish-grafana-dashboard.sh), and
# repo-agnostic in the same way: the folder is an input.
#
# Environment:
#   GRAFANA_FOLDER_UID  folder the rule group lives in (v-note: `v-note`)
#   GRAFANA_URL         e.g. https://grafana.desync.link   (not read by --dry-run)
#   GRAFANA_TOKEN       token with Edit on the folder      (not read by --dry-run)
#
# The request is `POST /api/ruler/grafana/api/v1/rules/<folder uid>` with the
# file as the body: Grafana replaces the whole group of that name in that
# folder, so the file is the complete truth for the group and a rule deleted
# from it is deleted from Grafana. Each rule's `grafana_alert.uid` is stable, so
# a republish updates the rule in place rather than creating another. The ruler
# API — not the provisioning API — because it is governed by folder permissions,
# which the CI service account already holds for the dashboard, and leaves the
# rules editable in the UI (where edits are overwritten by the next publish,
# exactly as for the dashboard).
#
# --dry-run prints the body and exits without touching the network.
#
# The token never appears in argv or in the output: curl reads the header from
# stdin, and a failure prints only the status and Grafana's response body.
set -eu

usage() {
  echo "usage: $0 [--dry-run] <rule-group.json>" >&2
  exit 2
}

DRY_RUN=false
if [ "${1:-}" = "--dry-run" ]; then
  DRY_RUN=true
  shift
fi
[ $# -eq 1 ] || usage
RULES_FILE="$1"

require() {
  eval "required_value=\${$1:-}"
  if [ -z "$required_value" ]; then
    echo "ERROR: $1 is required" >&2
    exit 1
  fi
}

require GRAFANA_FOLDER_UID
if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq is required" >&2
  exit 1
fi
if [ ! -f "$RULES_FILE" ]; then
  echo "ERROR: rule group file not found: $RULES_FILE" >&2
  exit 1
fi
# A named group of rules, each with a stable uid — without the uid every
# publish would add a duplicate rule.
if ! jq -e '(.name | type) == "string" and (.name | length) > 0
    and (.rules | type) == "array" and (.rules | length) > 0
    and all(.rules[]; (.grafana_alert.uid | type) == "string" and (.grafana_alert.uid | length) > 0)' \
    "$RULES_FILE" >/dev/null 2>&1; then
  echo "ERROR: $RULES_FILE is not a rule group with a name and a uid on every rule" >&2
  exit 1
fi

if [ "$DRY_RUN" = true ]; then
  jq . "$RULES_FILE"
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
jq . "$RULES_FILE" > "$WORK/payload.json"

URL="${GRAFANA_URL%/}/api/ruler/grafana/api/v1/rules/$GRAFANA_FOLDER_UID"
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
    echo "ERROR: Grafana answered HTTP $status to POST $URL for $RULES_FILE:" >&2
    cat "$WORK/response" >&2 2>/dev/null || true
    echo >&2
    exit 1
    ;;
esac

echo "published rule group $(jq -r .name "$RULES_FILE") ($(jq -r '.rules | length' "$RULES_FILE") rule(s)) to folder $GRAFANA_FOLDER_UID"
