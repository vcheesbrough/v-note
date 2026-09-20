#!/bin/sh
# Offline checks for the v-note Grafana dashboard and the script that publishes it.
#
# Every push that deploys dev publishes (deploy.yml `publish-grafana-dashboard`),
# so a broken dashboard would otherwise land straight on the shared dashboard.
# Everything here runs with no network and no token:
#
#   1. deploy/grafana/v-note-overview.json — parses, keeps its stable uid, commits
#      no numeric id, filters every query by `env`, uses one Prometheus datasource
#      by uid, charts only metrics the server actually registers, and never
#      mentions an unbounded id (page/session/owner/client batch).
#   2. scripts/publish-grafana-dashboard.sh — the --dry-run payload envelope, the
#      input guards, and the live path against a stub `curl`: a 2xx publishes,
#      a non-2xx or a transport failure fails loudly, and the token never reaches
#      argv or the output.
#
# POSIX sh + jq: runs in the `grafana-dashboard-validation` step on alpine.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DASHBOARD="$ROOT/deploy/grafana/v-note-overview.json"
PUBLISH="$ROOT/scripts/publish-grafana-dashboard.sh"
OBSERVABILITY_RS="$ROOT/crates/server/src/observability.rs"
PROMETHEUS_UID=PBFA97CFB590B2093
FAKE_TOKEN="fake-token-for-tests-3c9e1d"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
pass() { echo "  ok   — $1"; }
fail() { echo "  FAIL — $1" >&2; FAILURES=$((FAILURES + 1)); }

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required" >&2
  exit 1
fi

# Panels flattened out of rows, and the targets of each datasource type.
JQ_DEFS='
  def panels: [.panels[]? | ., (.panels[]?)];
  def targets: [panels[] | .targets[]?];
  def prom_exprs: [targets[] | select(.datasource.type == "prometheus") | .expr];
  def loki_exprs: [targets[] | select(.datasource.type == "loki") | .expr];
'

# `dash_check <description> <jq filter> [<jq filter explaining a failure>]`
dash_check() {
  if jq -e "$JQ_DEFS $2" "$DASHBOARD" >/dev/null 2>&1; then
    pass "$1"
  else
    fail "$1${3:+: $(jq -c "$JQ_DEFS $3" "$DASHBOARD" 2>&1)}"
  fi
}

echo "==> dashboard JSON"
if ! jq empty "$DASHBOARD" >/dev/null 2>&1; then
  fail "$DASHBOARD does not parse: $(jq empty "$DASHBOARD" 2>&1)"
  echo "grafana dashboard: $FAILURES check(s) failed" >&2
  exit 1
fi
pass "parses"

dash_check "stable uid v-note-overview" '.uid == "v-note-overview"' '.uid'
# Grafana's numeric id is per-instance; the publish sends id: null and the uid
# decides which dashboard is updated.
dash_check "no committed numeric id" '(.id // null) == null' '.id'
dash_check "description says UI edits are overwritten" \
  '.description | type == "string" and test("overwritten")' '.description'

# Pinned, not discovered (#392). As a label_values() query this would widen to
# whatever env series Prometheus happened to hold, so a second environment's
# metrics could appear on the dev dashboard without anyone changing this file.
# #388 copies the dashboard and changes the constant; the panels below stay put.
dash_check "env template variable is a constant pinned to dev" \
  '[.templating.list[] | select(.name == "env")] | length == 1 and
   (.[0].type == "constant") and
   ((.[0].query | if type == "object" then .query else . end) == "dev") and
   (.[0].current.value == "dev") and
   (.[0].hide == 2)' \
  '[.templating.list[] | select(.name == "env")]'

dash_check "panel ids are unique numbers" \
  'panels | map(.id) | all(type == "number") and length == (unique | length)' \
  'panels | map(.id)'
dash_check "every non-row panel has a title and an explicit datasource" \
  'panels | map(select(.type != "row")) | all((.title | length) > 0 and (.datasource.uid // "") != "")' \
  'panels | map(select(.type != "row" and ((.title // "") == "" or (.datasource.uid // "") == ""))) | map(.id)'
dash_check "every query target names its datasource" \
  'targets | length > 0 and all((.datasource.uid // "") != "")' \
  'targets | map(select((.datasource.uid // "") == "")) | map(.refId)'

# One way of choosing Prometheus, not a mix of uid and `${datasource}`.
dash_check "Prometheus is referenced by uid $PROMETHEUS_UID only" \
  '[.. | objects | select(.type? == "prometheus" and has("uid")) | .uid] | unique == ["'"$PROMETHEUS_UID"'"]' \
  '[.. | objects | select(.type? == "prometheus" and has("uid")) | .uid] | unique'
dash_check "no datasource template variable" \
  '[.templating.list[] | select(.type == "datasource")] | length == 0'

dash_check "every Prometheus metric selector filters env=\"\$env\"" \
  'prom_exprs | length > 0 and all([scan("v_note_[a-z_]+(?:\\{[^}]*\\})?")] | length > 0 and all(test("env=\"\\$env\"")))' \
  'prom_exprs | map(select([scan("v_note_[a-z_]+(?:\\{[^}]*\\})?")] | length == 0 or any(test("env=\"\\$env\"") | not)))'
dash_check "every Loki query filters env=\"\$env\"" \
  'loki_exprs | all(test("env=\"\\$env\""))' \
  'loki_exprs | map(select(test("env=\"\\$env\"") | not))'

# The whole document, not just exprs and legends: nothing on this dashboard has
# any business naming a per-user, per-page or per-session id.
dash_check "no reference to page_id, session_id, owner_id or client_batch_id" \
  'tostring | test("page_id|session_id|owner_id|client_batch_id") | not' \
  '[.. | strings | select(test("page_id|session_id|owner_id|client_batch_id"))]'

echo "==> dashboard charts the card's panel set"
for metric in \
  v_note_realtime_message_bytes_bucket v_note_realtime_message_bytes_count \
  v_note_realtime_replay_bytes_bucket v_note_realtime_replay_frames_bucket \
  v_note_realtime_replay_duration_seconds_bucket v_note_realtime_message_handling_seconds_bucket \
  v_note_realtime_active_connections v_note_realtime_events_total \
  v_note_http_requests_total v_note_http_request_duration_seconds_bucket \
  v_note_thumbnail_artifact_bytes_bucket v_note_thumbnail_generation_duration_seconds_bucket \
  v_note_thumbnail_generation_duration_seconds_count v_note_thumbnail_queue_depth \
  v_note_thumbnail_recoveries_total; do
  dash_check "charts $metric" "prom_exprs | any(test(\"$metric\\\\b\"))"
done
dash_check "lagged and *_error results are called out" \
  'prom_exprs | any(test("lagged") and test("_error"))'
dash_check "commit-batch handling latency has its own panel" \
  'panels | any((.targets // []) | length > 0 and all(.expr | test("message_handling_seconds_bucket") and test("commit-batch")))'

echo "==> every charted metric is one the server registers"
jq -r "$JQ_DEFS"' prom_exprs[] | scan("v_note_[a-z_]+")' "$DASHBOARD" \
  | sed -E 's/_(bucket|count|sum)$//' | sort -u > "$WORK/metrics"
while read -r metric; do
  if grep -qF "\"$metric\"" "$OBSERVABILITY_RS"; then
    pass "$metric is registered"
  else
    fail "$metric is charted but not registered in crates/server/src/observability.rs"
  fi
done < "$WORK/metrics"

echo "==> publish script"
# A stub curl records its argv, its stdin (the Authorization header) and the
# posted body, then answers with STUB_CURL_STATUS / STUB_CURL_BODY.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/curl" <<'STUB'
#!/bin/sh
echo "$*" >> "$CURL_CALLS"
cat > "$CURL_STDIN"
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --output) out="$2"; shift ;;
    --data-binary) cp "${2#@}" "$CURL_PAYLOAD"; shift ;;
  esac
  shift
done
[ "$STUB_CURL_EXIT" = 0 ] || exit "$STUB_CURL_EXIT"
printf '%s' "$STUB_CURL_BODY" > "$out"
printf '%s' "$STUB_CURL_STATUS"
STUB
chmod +x "$WORK/bin/curl"

CALLS="$WORK/curl-calls"
OUT="$WORK/out"

# `run_publish <mutation> <args...>` — runs the publish script with a CI-like
# environment after applying `mutation`. Echoes the exit code.
run_publish() {
  mutation="$1"
  shift
  : > "$CALLS"
  rm -f "$WORK/curl-stdin" "$WORK/curl-payload"
  rc=0
  (
    export PATH="$WORK/bin:$PATH"
    export CURL_CALLS="$CALLS" CURL_STDIN="$WORK/curl-stdin" CURL_PAYLOAD="$WORK/curl-payload"
    export STUB_CURL_EXIT=0 STUB_CURL_STATUS=200
    export STUB_CURL_BODY='{"id":7,"uid":"v-note-overview","url":"/d/v-note-overview/v-note-overview","status":"success","version":3}'
    export GRAFANA_URL=https://grafana.example.test/ GRAFANA_TOKEN="$FAKE_TOKEN"
    export GRAFANA_FOLDER_UID=v-note RELEASE_TAG=9.9.9 COMMIT_SHA=0123abcd
    unset GRAFANA_MESSAGE_PREFIX
    eval "$mutation"
    "$PUBLISH" "$@"
  ) > "$OUT" 2>&1 || rc=$?
  echo "$rc"
}

assert_no_curl() {
  if [ -s "$CALLS" ]; then
    fail "$1: called curl: $(cat "$CALLS")"
    return 1
  fi
}

# --- dry run -----------------------------------------------------------------
rc=$(run_publish "unset GRAFANA_URL GRAFANA_TOKEN" --dry-run "$DASHBOARD")
if [ "$rc" -ne 0 ]; then
  fail "dry run with no URL or token: exited $rc: $(cat "$OUT")"
else
  cp "$OUT" "$WORK/payload.json"
  assert_no_curl "dry run" && pass "dry run needs no URL or token and makes no request"
  if jq -e --slurpfile file "$DASHBOARD" '
      (keys == ["dashboard", "folderUid", "message", "overwrite"])
      and .folderUid == "v-note"
      and .overwrite == true
      and (.dashboard | has("id")) and .dashboard.id == null
      and .dashboard.uid == "v-note-overview"
      and .message == "v-note 9.9.9 0123abcd"
      and (.dashboard | del(.id)) == ($file[0] | del(.id))
    ' "$WORK/payload.json" >/dev/null 2>&1; then
    pass "dry-run payload envelope: folderUid v-note, overwrite true, dashboard.id null, message names release and sha"
  else
    fail "dry-run payload envelope: $(jq -c '{folderUid, overwrite, message, id: .dashboard.id, uid: .dashboard.uid, keys: keys}' "$WORK/payload.json" 2>&1)"
  fi
fi

jq '. + {id: 42}' "$DASHBOARD" > "$WORK/with-id.json"
rc=$(run_publish "true" --dry-run "$WORK/with-id.json")
if [ "$rc" -eq 0 ] && jq -e '.dashboard.id == null' "$OUT" >/dev/null 2>&1; then
  pass "a numeric id in the file is nulled in the payload"
else
  fail "a numeric id in the file is nulled in the payload: rc=$rc $(head -c 300 "$OUT")"
fi

# `export`: the base environment unsets this variable, so a bare assignment in
# the mutation would never reach the script.
rc=$(run_publish "export GRAFANA_MESSAGE_PREFIX=other-app" --dry-run "$DASHBOARD")
if [ "$rc" -eq 0 ] && jq -e '.message == "other-app 9.9.9 0123abcd"' "$OUT" >/dev/null 2>&1; then
  pass "the version-message prefix is an input (repo-agnostic)"
else
  fail "GRAFANA_MESSAGE_PREFIX is honoured: rc=$rc $(head -c 300 "$OUT")"
fi

# --- guards --------------------------------------------------------------------
# `assert_fails <description> <mutation> <expected message> <args...>`
assert_fails() {
  desc="$1" mutation="$2" expect_msg="$3"
  shift 3
  rc=$(run_publish "$mutation" "$@")
  if [ "$rc" -eq 0 ]; then
    fail "$desc: expected non-zero exit, got 0"
  elif ! grep -qF "$expect_msg" "$OUT"; then
    fail "$desc: exited $rc without '$expect_msg': $(cat "$OUT")"
  else
    assert_no_curl "$desc" && pass "$desc"
  fi
}

for name in GRAFANA_FOLDER_UID RELEASE_TAG COMMIT_SHA; do
  assert_fails "$name unset fails before any request" "unset $name" "ERROR: $name is required" "$DASHBOARD"
  assert_fails "$name blank fails before any request" "$name=''" "ERROR: $name is required" "$DASHBOARD"
done
for name in GRAFANA_URL GRAFANA_TOKEN; do
  assert_fails "$name unset fails a live publish before any request" "unset $name" \
    "ERROR: $name is required" "$DASHBOARD"
done
assert_fails "a missing dashboard file fails" "true" "dashboard file not found" "$WORK/nope.json"
echo '{"title":"no uid"}' > "$WORK/no-uid.json"
assert_fails "a dashboard without a uid fails" "true" "non-empty uid" "$WORK/no-uid.json"
echo '{not json' > "$WORK/broken.json"
assert_fails "a dashboard that does not parse fails" "true" "non-empty uid" "$WORK/broken.json"
assert_fails "no dashboard argument is a usage error" "true" "usage:"

# --- live publish against the stub ---------------------------------------------
rc=$(run_publish "true" "$DASHBOARD")
if [ "$rc" -ne 0 ]; then
  fail "a 200 publishes: exited $rc: $(cat "$OUT")"
else
  pass "a 200 publishes"
  if grep -qF "POST" "$CALLS" && grep -qF "https://grafana.example.test/api/dashboards/db" "$CALLS"; then
    pass "POSTs to /api/dashboards/db, trailing slash on GRAFANA_URL tolerated"
  else
    fail "request line: $(cat "$CALLS")"
  fi
  if jq -e --slurpfile dry "$WORK/payload.json" '. == $dry[0]' "$WORK/curl-payload" >/dev/null 2>&1; then
    pass "the posted body is exactly the dry-run payload"
  else
    fail "the posted body differs from the dry-run payload"
  fi
  if grep -qF "$FAKE_TOKEN" "$CALLS"; then
    fail "the token appeared in curl's argv"
  else
    pass "the token is not in curl's argv"
  fi
  if grep -qxF "Authorization: Bearer $FAKE_TOKEN" "$WORK/curl-stdin"; then
    pass "the Authorization header reaches curl on stdin"
  else
    fail "the Authorization header did not reach curl on stdin"
  fi
  if grep -qF "version 3" "$OUT" && grep -qF "v-note 9.9.9 0123abcd" "$OUT"; then
    pass "success output names the new version and the version message"
  else
    fail "success output: $(cat "$OUT")"
  fi
fi

for status in 400 403 412 500; do
  rc=$(run_publish "STUB_CURL_STATUS=$status STUB_CURL_BODY='{\"message\":\"stub rejection $status\"}'" "$DASHBOARD")
  if [ "$rc" -eq 0 ]; then
    fail "HTTP $status fails the publish: exited 0"
  elif ! grep -qF "HTTP $status" "$OUT" || ! grep -qF "stub rejection $status" "$OUT"; then
    fail "HTTP $status prints the status and Grafana's body: $(cat "$OUT")"
  elif grep -qF "$FAKE_TOKEN" "$OUT"; then
    fail "HTTP $status output leaked the token"
  else
    pass "HTTP $status fails loudly with Grafana's body and no token"
  fi
done

rc=$(run_publish "STUB_CURL_EXIT=7" "$DASHBOARD")
if [ "$rc" -ne 0 ] && grep -qF "failed before Grafana answered" "$OUT" && ! grep -qF "$FAKE_TOKEN" "$OUT"; then
  pass "a transport failure fails the publish without leaking the token"
else
  fail "transport failure: rc=$rc $(cat "$OUT")"
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "grafana dashboard: $FAILURES check(s) failed" >&2
  exit 1
fi
echo "grafana dashboard + publish script OK"
