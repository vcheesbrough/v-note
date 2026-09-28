#!/bin/sh
# Offline checks for the one Alloy config v-note still commits: the e2e
# stand-in for the shared monitoring Alloy (e2e/alloy/monitor-alloy.alloy).
#
# Until #439 this validated deploy/alloy/client-telemetry.alloy, the client
# telemetry sidecar. That sidecar is gone — client telemetry goes through the
# otlp-collector-oidc image, which carries its own tests — and no deployed
# artifact of v-note is an Alloy config any more. What is left is the fixture
# every e2e run depends on, and this is the fast check that it still parses:
#
#   1. Every file that names the Alloy image names the same one, so the fixture
#      is validated by the version that runs it.
#   2. `alloy fmt` is a no-op on the file.
#   3. `alloy validate` accepts it — and rejects a deliberately broken copy, so
#      a validate that has quietly stopped validating is caught here rather than
#      believed.
#   4. The properties the server-telemetry and client-telemetry specs rely on
#      (#417, #439) cannot quietly change under them.
#
# POSIX sh. Runs in the `alloy-config-validation` step on the Alloy image itself,
# which has `alloy` on PATH; locally it falls back to running that same image
# with docker.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG="$ROOT/e2e/alloy/monitor-alloy.alloy"
E2E_COMPOSE="$ROOT/e2e/docker-compose.test.yml"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
pass() { echo "  ok   — $1"; }
fail() { echo "  FAIL — $1" >&2; FAILURES=$((FAILURES + 1)); }

# The refs as written, e.g. `grafana/alloy:v1.17.1@sha256:…`. Matched on the
# repository name rather than on a YAML key: checks.yml names it under `image:`
# and the e2e stack under a Dockerfile `FROM`.
image_refs() {
  grep -o 'grafana/alloy:[A-Za-z0-9._-]*@sha256:[0-9a-f]\{64\}' "$1" || true
}

echo "==> one Alloy image, everywhere it is named"
IMAGE="$(image_refs "$E2E_COMPOSE" | sort -u)"
if [ "$(printf '%s\n' "$IMAGE" | grep -c .)" -ne 1 ]; then
  echo "ERROR: expected exactly one digest-pinned grafana/alloy ref in e2e/docker-compose.test.yml, found: '$IMAGE'" >&2
  exit 1
fi
pass "e2e/docker-compose.test.yml pins $IMAGE"
refs="$(image_refs "$ROOT/.woodpecker/checks.yml" | sort -u)"
if [ "$refs" = "$IMAGE" ]; then
  pass ".woodpecker/checks.yml agrees"
else
  fail ".woodpecker/checks.yml pins a different Alloy than the e2e stack: '$refs'"
fi

if command -v alloy >/dev/null 2>&1; then
  run_alloy() { sub="$1"; file="$2"; alloy "$sub" "$file"; }
elif command -v docker >/dev/null 2>&1; then
  run_alloy() {
    sub="$1"; file="$2"
    docker run --rm -e E2E_COMPOSE_PROJECT -v "$file:/config.alloy:ro" "$IMAGE" "$sub" /config.alloy
  }
else
  echo "ERROR: neither alloy nor docker is available" >&2
  exit 1
fi

# The value the fixture reads through sys.env().
export E2E_COMPOSE_PROJECT=validation

echo "==> alloy fmt"
if run_alloy fmt "$CONFIG" > "$WORK/formatted.alloy" 2> "$WORK/fmt.err"; then
  if diff -u "$CONFIG" "$WORK/formatted.alloy" > "$WORK/fmt.diff"; then
    pass "already formatted"
  else
    fail "not formatted — apply this diff: $(cat "$WORK/fmt.diff")"
  fi
else
  fail "alloy fmt failed: $(cat "$WORK/fmt.err")"
fi

echo "==> alloy validate"
if run_alloy validate "$CONFIG" > "$WORK/validate.out" 2>&1; then
  pass "the committed fixture validates"
else
  fail "the committed fixture does not validate: $(cat "$WORK/validate.out")"
fi
# Negative control: route the receiver's traces to a component that does not
# exist. If this passes, `validate` is not checking the graph.
sed 's/otelcol\.processor\.transform\.apps\.input/otelcol.processor.transform.no_such_component.input/' \
  "$CONFIG" > "$WORK/broken.alloy"
if cmp -s "$CONFIG" "$WORK/broken.alloy"; then
  fail "negative control did not change the file — update its sed pattern"
elif run_alloy validate "$WORK/broken.alloy" > "$WORK/broken.out" 2>&1; then
  fail "a config wired to a nonexistent component validated — validate is not validating"
else
  pass "a config wired to a nonexistent component is rejected"
fi

echo "==> e2e shared-Alloy stand-in: what the server-telemetry spec relies on"
CODE="$WORK/standin-code.alloy"
grep -v '^[[:space:]]*//' "$CONFIG" > "$CODE"
# The shared Alloy's `apps` receiver listens on both OTLP ports (config.alloy),
# so the stand-in does too; the server exports OTLP/gRPC to :4317.
for port in 4317 4318; do
  if grep -q "endpoint = \"0\.0\.0\.0:$port\"" "$CODE"; then
    pass "the apps receiver listens on :$port"
  else
    fail "the apps receiver does not listen on :$port"
  fi
done
# Without a logs output the receiver accepts OTLP logs and drops them — which is
# what the real shared Alloy did before mini-config #47.
if grep -Eq '^[[:space:]]*logs[[:space:]]*=[[:space:]]*\[otelcol\.processor\.transform\.apps\.input\]' "$CODE"; then
  pass "the apps receiver routes logs through the vocabulary translation"
else
  fail "the apps receiver does not route logs through otelcol.processor.transform.apps"
fi
# The shared Alloy (mini-config #47) stamps no marker of its own: the pusher
# marks its data, and `telemetry_source` fills the indexed `log_source`. A
# stand-in that stamped `otlp` itself would let e2e pass on a server that sets
# no marker — the exact fault dev would then show.
if grep -q 'set(attributes\["log_source"\], attributes\["telemetry_source"\]) where attributes\["log_source"\] == nil' "$CODE"; then
  pass "telemetry_source fills log_source where absent, as in the shared Alloy"
else
  fail "the stand-in does not translate telemetry_source into log_source as config.alloy does"
fi
stamped="$(grep -E 'set\((resource\.)?attributes\["(log_source|telemetry_source)"\], "(otlp|docker|file)"\)' "$CODE" || true)"
if [ -z "$stamped" ]; then
  pass "the collector stamps no server-side marker of its own"
else
  fail "the stand-in stamps a marker the pusher should set: $stamped"
fi
if grep -q '"log_source" = "docker"' "$CODE"; then
  pass "the Docker scrape marks its lines log_source=docker"
else
  fail "the Docker scrape does not set log_source=docker"
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "alloy config validation FAILED ($FAILURES)" >&2
  exit 1
fi
echo "alloy config validation OK"
