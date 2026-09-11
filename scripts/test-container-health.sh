#!/bin/sh
# Verify the HEALTHCHECK baked into the web image by Dockerfile.web.
#
# Three things depend on that healthcheck and none of them can tell you it is
# wrong: the deploy gate (scripts/deploy-v-note.sh polls the container's status
# instead of probing the app itself), the e2e stack's `service_healthy`
# conditions, and whoever reads `docker ps` on mini. A healthcheck that silently
# stopped working would make all three report green — the deploy gate would wait
# for a status that never becomes unhealthy, and e2e would hang rather than fail.
#
# So this asserts both halves: that the image carries the probe we think it does,
# and that the probe actually goes unhealthy when the endpoint is gone.
set -eu

IMAGE="${1:?usage: test-container-health.sh <image-ref>}"

# Mirrors Dockerfile.web deliberately. Changing the probe should be a considered
# two-file edit, not something a stray character in a RUN line can do quietly.
EXPECTED_TEST='[CMD-SHELL curl -fsS -k --max-time 3 https://127.0.0.1:443/health || exit 1]'
EXPECTED_INTERVAL=30000000000      # 30s
EXPECTED_START_INTERVAL=1000000000 #  1s
EXPECTED_TIMEOUT=3000000000        #  3s
EXPECTED_START_PERIOD=60000000000  # 60s
EXPECTED_RETRIES=3

FAILURES=0
pass() { echo "  ok   — $1"; }
fail() { echo "  FAIL — $1" >&2; FAILURES=$((FAILURES + 1)); }

assert_eq() {
  desc="$1"
  actual="$2"
  expected="$3"
  if [ "$actual" = "$expected" ]; then
    pass "$desc"
  else
    fail "$desc: got '$actual', expected '$expected'"
  fi
}

field() {
  docker image inspect --format "{{ $1 }}" "$IMAGE"
}

# The durations are read out of the marshalled blob rather than through a Go
# template for two reasons: `{{.Config.Healthcheck.Interval}}` renders as raw
# nanoseconds on older daemons and as "30s" on newer ones, and StartInterval does
# not exist at all before daemon 25 — where a template naming it dies with a
# template error instead of a readable assertion. The JSON is int64 nanoseconds
# everywhere. The `[,{]` prefix matters: a greedy `.*"Interval":` would happily
# match the *StartInterval* key.
json_field() {
  docker image inspect --format '{{json .Config.Healthcheck}}' "$IMAGE" \
    | sed -n 's/.*[,{]"'"$1"'":\([0-9]*\).*/\1/p'
}

echo "==> HEALTHCHECK config on $IMAGE"

if [ "$(docker image inspect --format '{{if .Config.Healthcheck}}yes{{else}}no{{end}}' "$IMAGE")" != "yes" ]; then
  echo "ERROR: $IMAGE has no HEALTHCHECK — the deploy gate and the e2e" >&2
  echo "       service_healthy conditions both depend on it" >&2
  exit 1
fi

assert_eq "probes /health over https on 443" "$(field '.Config.Healthcheck.Test')" "$EXPECTED_TEST"
assert_eq "interval"       "$(json_field Interval)"      "$EXPECTED_INTERVAL"
assert_eq "start interval" "$(json_field StartInterval)" "$EXPECTED_START_INTERVAL"
assert_eq "timeout"        "$(json_field Timeout)"       "$EXPECTED_TIMEOUT"
assert_eq "start period"   "$(json_field StartPeriod)"   "$EXPECTED_START_PERIOD"
assert_eq "retries"        "$(json_field Retries)"       "$EXPECTED_RETRIES"

# Asserting the config proves what the probe *says*, not that it works. Run the
# image with the server replaced by a sleep: the baked-in probe still runs, but
# nothing is listening on 443. The timing flags override only the durations —
# the probe command still comes from the image — so a transition that takes ~90s
# in production takes a few seconds here.
echo "==> an unreachable endpoint goes unhealthy"

CONTAINER=""
cleanup() {
  [ -n "$CONTAINER" ] && docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

CONTAINER=$(docker run -d \
  --entrypoint sleep \
  --health-interval=1s \
  --health-timeout=2s \
  --health-retries=2 \
  --health-start-period=1s \
  "$IMAGE" 60)

STATUS=""
DEADLINE=$(( $(date +%s) + 30 ))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  STATUS=$(docker inspect -f '{{.State.Health.Status}}' "$CONTAINER")
  [ "$STATUS" = "unhealthy" ] && break
  sleep 1
done

assert_eq "container with no server on 443 reports unhealthy" "$STATUS" "unhealthy"

# A missing curl would also produce "unhealthy", for entirely the wrong reason.
# The probe's own output is what separates "the endpoint is down" from "the image
# cannot probe anything at all".
PROBE_OUTPUT=$(docker inspect -f '{{range .State.Health.Log}}{{.Output}}{{end}}' "$CONTAINER")
if echo "$PROBE_OUTPUT" | grep -q 'curl:'; then
  pass "the probe ran curl and curl reported the connection failure"
else
  fail "probe output does not look like a curl error (is curl installed?): $PROBE_OUTPUT"
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "container health check: $FAILURES check(s) failed" >&2
  exit 1
fi
echo "==> OK"
