#!/bin/sh
# Offline checks for the v-note Grafana dashboard and the script that publishes it.
#
# Every dev deployment publishes (deploy.yml `publish-grafana-dashboard`),
# so a broken dashboard would otherwise land straight on the shared dashboard.
# Everything here runs with no network and no token:
#
#   1. deploy/grafana/v-note-overview.json — parses, keeps its stable uid, commits
#      no numeric id, filters every query by `deployment_environment`, uses one Prometheus datasource
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
# #439: the client telemetry ingest's metrics, as the pinned image exports them
# (proved by e2e/tests/client-telemetry.spec.ts), and the alert rules on them.
INGEST_METRICS="$ROOT/deploy/grafana/otlp-collector-oidc-metrics.txt"
ALERTS="$ROOT/deploy/grafana/v-note-alerts.json"
PUBLISH_ALERTS="$ROOT/scripts/publish-grafana-alerts.sh"
INGEST_SERVICE=v-note-otlp-collector-oidc
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

# One label name for every signal. mini-config's Alloy attaches the
# observability.deployment.environment container label as `deployment_environment` on scraped
# metrics and Docker logs — the name Loki gives the `deployment.environment`
# resource attribute on OTLP ingest (client telemetry, #354) — so the same
# selector covers both. Anchored so it is not satisfied by a longer label that
# merely ends in the same letters, and so a leftover bare `env=` fails.
dash_check "every Prometheus metric selector filters deployment_environment=\"\$env\"" \
  'prom_exprs | length > 0 and all([scan("(?:v_note|otelcol)_[a-z_]+(?:\\{[^}]*\\})?")] | length > 0 and all(test("[{,[:space:]]deployment_environment=\"\\$env\"")))' \
  'prom_exprs | map(select([scan("(?:v_note|otelcol)_[a-z_]+(?:\\{[^}]*\\})?")] | length == 0 or any(test("[{,[:space:]]deployment_environment=\"\\$env\"") | not)))'
# The ingest's `otelcol_*` names are every collector's names: without the
# service filter a panel would sum another product's ingest (or the shared
# Alloy's own collector metrics) into v-note's.
dash_check "every otelcol_* selector is scoped to service_name=\"$INGEST_SERVICE\"" \
  'prom_exprs | all([scan("otelcol_[a-z_]+(?:\\{[^}]*\\})?")] | all(test("[{,[:space:]]service_name=\"'"$INGEST_SERVICE"'\"")))' \
  'prom_exprs | map(select([scan("otelcol_[a-z_]+(?:\\{[^}]*\\})?")] | any(test("service_name=\"'"$INGEST_SERVICE"'\"") | not)))'
# Client streams arrive over OTLP from the ingest, which stamps the contract's
# `deployment.environment.name` (Loki: `deployment_environment_name`); the
# server's arrive by the Docker scrape as `deployment_environment`. Either names
# the environment; one of them must be there.
dash_check "every Loki query filters deployment_environment(_name)=\"\$env\"" \
  'loki_exprs | all(test("[{,[:space:]]deployment_environment(_name)?=\"\\$env\""))' \
  'loki_exprs | map(select(test("[{,[:space:]]deployment_environment(_name)?=\"\\$env\"") | not))'
dash_check "no query filters the retired env label" \
  '(prom_exprs + loki_exprs) | all(test("[{,[:space:]]env=") | not)' \
  '(prom_exprs + loki_exprs) | map(select(test("[{,[:space:]]env=")))'

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
  v_note_thumbnail_recoveries_total \
  otelcol_receiver_accepted_spans otelcol_receiver_accepted_log_records \
  otelcol_oidcclientauth_rejections otelcol_processor_filter_spans_filtered; do
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

echo "==> every charted or alerted ingest metric is one the pinned image exports"
{
  jq -r "$JQ_DEFS"' prom_exprs[] | scan("otelcol_[a-z_]+")' "$DASHBOARD"
  jq -r '.. | .expr? // empty | scan("otelcol_[a-z_]+")' "$ALERTS"
} | sort -u > "$WORK/ingest-metrics"
if [ ! -s "$WORK/ingest-metrics" ]; then
  fail "no otelcol_* metric is charted or alerted on — the Client telemetry row is gone"
fi
while read -r metric; do
  if grep -qx "$metric" "$INGEST_METRICS"; then
    pass "$metric is in otlp-collector-oidc-metrics.txt"
  else
    fail "$metric is queried but not in deploy/grafana/otlp-collector-oidc-metrics.txt (which e2e proves against the pinned image)"
  fi
done < "$WORK/ingest-metrics"

echo "==> alert rules (#439)"
if ! jq empty "$ALERTS" >/dev/null 2>&1; then
  fail "$ALERTS does not parse"
else
  pass "alert rules parse"
  alert_check() {
    if jq -e "$2" "$ALERTS" >/dev/null 2>&1; then pass "$1"; else fail "$1: $(jq -c "${3:-.}" "$ALERTS" 2>&1 | head -c 400)"; fi
  }
  alert_check "a named group with at least one rule" '(.name | length) > 0 and (.rules | length) > 0'
  alert_check "every rule has a stable uid, a title and a unique uid" \
    '[.rules[].grafana_alert.uid] | all(type == "string" and length > 0) and length == (unique | length)'
  alert_check "every rule has a runbook link" \
    'all(.rules[]; (.annotations.runbook_url // "") | startswith("https://"))' '[.rules[].annotations]'
  alert_check "every rule has a severity" 'all(.rules[]; (.labels.severity // "") | test("^(critical|warning)$"))'
  alert_check "every Prometheus query uses the one Prometheus datasource" \
    '[.rules[].grafana_alert.data[] | select(.datasourceUid != "__expr__") | .datasourceUid] | unique == ["'"$PROMETHEUS_UID"'"]'
  alert_check "every Prometheus query is pinned to deployment_environment=\"dev\" and the ingest's service" \
    'all(.rules[].grafana_alert.data[] | select(.datasourceUid != "__expr__") | .model.expr; test("deployment_environment=\"dev\"") and test("service_name=\"'"$INGEST_SERVICE"'\""))'
  alert_check "the ingest-volume alert exists (client-ingest.md, Operating it)" \
    'any(.rules[]; .grafana_alert.uid == "v-note-client-ingest-volume" and (.grafana_alert.data[0].model.expr | test("otelcol_receiver_accepted_spans") and test("otelcol_receiver_accepted_log_records")))'
fi

echo "==> alert publish script"
rc=0
( unset GRAFANA_URL GRAFANA_TOKEN; GRAFANA_FOLDER_UID=v-note "$PUBLISH_ALERTS" --dry-run "$ALERTS" ) > "$WORK/alerts-dry" 2>&1 || rc=$?
if [ "$rc" -eq 0 ] && jq -e --slurpfile file "$ALERTS" '. == $file[0]' "$WORK/alerts-dry" >/dev/null 2>&1; then
  pass "alert dry run needs no URL or token, and its body is the file"
else
  fail "alert dry run: rc=$rc $(head -c 300 "$WORK/alerts-dry")"
fi
echo '{"name":"g","rules":[{"grafana_alert":{"title":"no uid"}}]}' > "$WORK/no-uid-rules.json"
rc=0
GRAFANA_FOLDER_UID=v-note "$PUBLISH_ALERTS" --dry-run "$WORK/no-uid-rules.json" > "$WORK/alerts-out" 2>&1 || rc=$?
if [ "$rc" -ne 0 ] && grep -qF "uid on every rule" "$WORK/alerts-out"; then
  pass "a rule without a uid is refused"
else
  fail "a rule without a uid: rc=$rc $(cat "$WORK/alerts-out")"
fi
rc=0
( unset GRAFANA_FOLDER_UID; "$PUBLISH_ALERTS" --dry-run "$ALERTS" ) > "$WORK/alerts-out" 2>&1 || rc=$?
if [ "$rc" -ne 0 ] && grep -qF "GRAFANA_FOLDER_UID is required" "$WORK/alerts-out"; then
  pass "the folder is required"
else
  fail "folder guard: rc=$rc $(cat "$WORK/alerts-out")"
fi

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

# The alert publish, live against the same stub curl.
: > "$CALLS"
rc=0
(
  export PATH="$WORK/bin:$PATH"
  export CURL_CALLS="$CALLS" CURL_STDIN="$WORK/curl-stdin" CURL_PAYLOAD="$WORK/curl-payload"
  export STUB_CURL_EXIT=0 STUB_CURL_STATUS=202 STUB_CURL_BODY='{"message":"rule group updated successfully"}'
  export GRAFANA_URL=https://grafana.example.test/ GRAFANA_TOKEN="$FAKE_TOKEN" GRAFANA_FOLDER_UID=v-note
  "$PUBLISH_ALERTS" "$ALERTS"
) > "$OUT" 2>&1 || rc=$?
if [ "$rc" -eq 0 ] && grep -qF "https://grafana.example.test/api/ruler/grafana/api/v1/rules/v-note" "$CALLS" \
  && jq -e --slurpfile file "$ALERTS" '. == $file[0]' "$WORK/curl-payload" >/dev/null 2>&1 \
  && ! grep -qF "$FAKE_TOKEN" "$CALLS" "$OUT" \
  && grep -qxF "Authorization: Bearer $FAKE_TOKEN" "$WORK/curl-stdin"; then
  pass "alerts POST to the folder's ruler group, the file as body, the token on stdin only"
else
  fail "alert publish: rc=$rc calls=$(cat "$CALLS") out=$(cat "$OUT")"
fi
rc=0
(
  export PATH="$WORK/bin:$PATH"
  export CURL_CALLS="$CALLS" CURL_STDIN="$WORK/curl-stdin" CURL_PAYLOAD="$WORK/curl-payload"
  export STUB_CURL_EXIT=0 STUB_CURL_STATUS=403 STUB_CURL_BODY='{"message":"stub forbidden"}'
  export GRAFANA_URL=https://grafana.example.test GRAFANA_TOKEN="$FAKE_TOKEN" GRAFANA_FOLDER_UID=v-note
  "$PUBLISH_ALERTS" "$ALERTS"
) > "$OUT" 2>&1 || rc=$?
if [ "$rc" -ne 0 ] && grep -qF "HTTP 403" "$OUT" && grep -qF "stub forbidden" "$OUT" && ! grep -qF "$FAKE_TOKEN" "$OUT"; then
  pass "an alert publish refusal fails loudly with Grafana's body and no token"
else
  fail "alert publish refusal: rc=$rc $(cat "$OUT")"
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "grafana dashboard: $FAILURES check(s) failed" >&2
  exit 1
fi
echo "grafana dashboard + publish script OK"
