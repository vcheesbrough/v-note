#!/bin/sh
# Offline checks for the Alloy configs this repo carries: the client telemetry
# sidecar (#354), which ships, and the e2e stand-in for the shared monitoring
# Alloy (#417), which does not — see the stand-in section at the end.
#
# deploy/alloy/client-telemetry.alloy is applied by CI to a running environment
# and is the only thing standing between a client's claims about itself and what
# Tempo and Loki store. e2e asserts the behaviour against real backends; this is
# the fast half, and the half that still runs when e2e is red for another reason:
#
#   1. Every file that names the Alloy image names the same one. The digest is
#      repeated (compose cannot share an anchor across files), and a sidecar
#      validated on one version and run on another validates nothing.
#   2. `alloy fmt` is a no-op on the file.
#   3. `alloy validate` accepts it — and rejects a deliberately broken copy, so
#      a validate that has quietly stopped validating is caught here rather than
#      believed.
#   4. The properties that make the file a security control, not just a pipeline,
#      survive a well-meaning edit. See each check for the failure it prevents.
#
# POSIX sh. Runs in the `alloy-config-validation` step on the Alloy image itself,
# which has `alloy` on PATH; locally it falls back to running that same image
# with docker, so neither place can check against a different version.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG="$ROOT/deploy/alloy/client-telemetry.alloy"
STANDIN="$ROOT/e2e/alloy/monitor-alloy.alloy"
DEPLOY_COMPOSE="$ROOT/deploy/docker-compose.yml"

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
IMAGE="$(image_refs "$DEPLOY_COMPOSE" | sort -u)"
if [ "$(printf '%s\n' "$IMAGE" | grep -c .)" -ne 1 ]; then
  echo "ERROR: expected exactly one digest-pinned grafana/alloy ref in deploy/docker-compose.yml, found: '$IMAGE'" >&2
  exit 1
fi
pass "deploy/docker-compose.yml pins $IMAGE"
for file in e2e/docker-compose.test.yml .woodpecker/checks.yml; do
  refs="$(image_refs "$ROOT/$file" | sort -u)"
  if [ -z "$refs" ]; then
    fail "$file names no digest-pinned grafana/alloy image"
  elif [ "$refs" != "$IMAGE" ]; then
    fail "$file pins a different Alloy than deploy/docker-compose.yml: $refs"
  else
    pass "$file agrees"
  fi
done

# `alloy <subcommand> <file>`, against the pinned version in both environments.
if command -v alloy >/dev/null 2>&1; then
  run_alloy() { sub="$1"; file="$2"; alloy "$sub" "$file"; }
elif command -v docker >/dev/null 2>&1; then
  run_alloy() {
    sub="$1"; file="$2"
    docker run --rm \
      -e APP_ENV -e APP_VERSION \
      -e CLIENT_TELEMETRY_TEMPO_ENDPOINT -e CLIENT_TELEMETRY_LOKI_ENDPOINT \
      -e E2E_COMPOSE_PROJECT \
      -v "$file:/config.alloy:ro" "$IMAGE" "$sub" /config.alloy
  }
else
  echo "ERROR: neither alloy nor docker is available" >&2
  exit 1
fi

# The values the config reads through sys.env(). An unset one resolves to "",
# which is a valid string but not a valid exporter endpoint.
export APP_ENV=validation APP_VERSION=0.0.0
export CLIENT_TELEMETRY_TEMPO_ENDPOINT=tempo.invalid:4317
export CLIENT_TELEMETRY_LOKI_ENDPOINT=http://loki.invalid:3100/otlp
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
  pass "the committed config validates"
else
  fail "the committed config does not validate: $(cat "$WORK/validate.out")"
fi
# Negative control: route a receiver's output to a component that does not
# exist. If this passes, `validate` is not checking the graph and the result
# above means nothing.
sed 's/otelcol\.processor\.transform\.spa\.input/otelcol.processor.transform.no_such_component.input/' \
  "$CONFIG" > "$WORK/broken.alloy"
if cmp -s "$CONFIG" "$WORK/broken.alloy"; then
  fail "negative control did not change the file — update its sed pattern"
elif run_alloy validate "$WORK/broken.alloy" > "$WORK/broken.out" 2>&1; then
  fail "a config wired to a nonexistent component validated — validate is not validating"
else
  pass "a config wired to a nonexistent component is rejected"
fi

echo "==> the properties that make this a security control"
# Checked against the code alone. The file explains itself at length, and a
# comment that quotes a statement would otherwise be counted as one — which is
# how a check like this ends up passing on a config that lost the real thing.
CODE="$WORK/code.alloy"
grep -v '^[[:space:]]*//' "$CONFIG" > "$CODE"
# A `set` overwrites the attributes it names and waves every other one through.
# Loki's OTLP ingest promotes resource attributes such as service.instance.id to
# index labels, so without the allow-list a client can mint a stream per request.
# Two client kinds x two signals = four statement blocks, each needing its own.
keep_keys_count="$(grep -c 'keep_keys(resource\.attributes' "$CODE" || true)"
if [ "$keep_keys_count" -eq 4 ]; then
  pass "every statement block allow-lists resource attributes before setting them"
else
  fail "expected 4 keep_keys(resource.attributes, …) statements (2 clients x traces+logs), found $keep_keys_count"
fi

# The default, `ignore`, logs a failed statement and forwards the record
# unmodified — i.e. carrying whatever the client claimed.
propagate_count="$(grep -c 'error_mode[[:space:]]*=[[:space:]]*"propagate"' "$CODE" || true)"
transform_count="$(grep -c '^otelcol\.processor\.transform ' "$CODE" || true)"
if [ "$transform_count" -ge 1 ] && [ "$propagate_count" -eq "$transform_count" ]; then
  pass "every transform drops a record it failed to rewrite, rather than forwarding it"
else
  fail "$transform_count transform(s) but $propagate_count with error_mode = \"propagate\""
fi

# Distinct identities per client kind are the reason there are two receivers.
for name in v-note-spa v-note-android; do
  if grep -q "set(resource\.attributes\[\"service\.name\"\], \"$name\")" "$CODE"; then
    pass "service.name is forced to $name"
  else
    fail "nothing forces service.name to $name"
  fi
done
# …and neither may pass itself off as the server, whose service.name is `v-note`.
if grep -q 'set(resource\.attributes\["service\.name"\], "v-note")' "$CODE"; then
  fail "a client pipeline forces service.name to the server's own name"
else
  pass "no client pipeline can be labelled as the server"
fi

# The estate's path marker, `client` on every signal, is what lets a reader tell
# a span or line a user's device sent from one the server vouches for. `otlp` is
# the server's own value, and `docker`/`file` are the platform's: writing any of
# them here is the forgery this pipeline exists to prevent, made by the config
# instead of the client. Four blocks, as for keep_keys.
marker_count="$(grep -c 'set(resource\.attributes\["log_source"\], "client")' "$CODE" || true)"
if [ "$marker_count" -eq 4 ]; then
  pass "every statement block marks the record as client-origin"
else
  fail "expected 4 set(resource.attributes[\"log_source\"], \"client\") statements (2 clients x traces+logs), found $marker_count"
fi
other_marker="$(grep 'set(resource\.attributes\["log_source"\]' "$CODE" | grep -v '"client")' || true)"
if [ -z "$other_marker" ]; then
  pass "no statement block passes client telemetry off as another source"
else
  fail "a statement block sets log_source to something other than \"client\": $other_marker"
fi

# Client metrics were dropped from #354. With no metrics output the receiver
# answers 404 on /v1/metrics; wiring one makes it accept data this pipeline then
# has nowhere to send.
if grep -Eq '^[[:space:]]*metrics[[:space:]]*=' "$CODE"; then
  fail "a metrics output is wired — client metrics are out of scope and have no exporter"
else
  pass "no metrics output is wired, so /v1/metrics stays a 404"
fi

# ---------------------------------------------------------------------------
# The e2e stand-in for the shared monitoring Alloy (#417)
# ---------------------------------------------------------------------------
# e2e/alloy/monitor-alloy.alloy is a fixture: it mirrors mini-config's shared
# Alloy so the server's own OTLP export is tested against the collector shape it
# is deployed against. It is validated here so a broken fixture fails in
# seconds rather than as an e2e timeout, and so the properties the e2e
# assertions depend on cannot quietly change under them.
echo "==> e2e shared-Alloy stand-in: alloy fmt"
if run_alloy fmt "$STANDIN" > "$WORK/standin-formatted.alloy" 2> "$WORK/standin-fmt.err"; then
  if diff -u "$STANDIN" "$WORK/standin-formatted.alloy" > "$WORK/standin-fmt.diff"; then
    pass "already formatted"
  else
    fail "not formatted — apply this diff: $(cat "$WORK/standin-fmt.diff")"
  fi
else
  fail "alloy fmt failed: $(cat "$WORK/standin-fmt.err")"
fi

echo "==> e2e shared-Alloy stand-in: alloy validate"
if run_alloy validate "$STANDIN" > "$WORK/standin-validate.out" 2>&1; then
  pass "the stand-in validates"
else
  fail "the stand-in does not validate: $(cat "$WORK/standin-validate.out")"
fi
sed 's/otelcol\.processor\.transform\.apps\.input/otelcol.processor.transform.no_such_component.input/' \
  "$STANDIN" > "$WORK/standin-broken.alloy"
if cmp -s "$STANDIN" "$WORK/standin-broken.alloy"; then
  fail "stand-in negative control did not change the file — update its sed pattern"
elif run_alloy validate "$WORK/standin-broken.alloy" > "$WORK/standin-broken.out" 2>&1; then
  fail "a stand-in wired to a nonexistent component validated — validate is not validating"
else
  pass "a stand-in wired to a nonexistent component is rejected"
fi

echo "==> e2e shared-Alloy stand-in: what the server-telemetry spec relies on"
STANDIN_CODE="$WORK/standin-code.alloy"
grep -v '^[[:space:]]*//' "$STANDIN" > "$STANDIN_CODE"
# The shared Alloy's `apps` receiver listens on both OTLP ports (config.alloy),
# so the stand-in does too; the server exports OTLP/gRPC to :4317.
for port in 4317 4318; do
  if grep -q "endpoint = \"0\.0\.0\.0:$port\"" "$STANDIN_CODE"; then
    pass "the apps receiver listens on :$port"
  else
    fail "the apps receiver does not listen on :$port"
  fi
done
# Without a logs output the receiver accepts OTLP logs and drops them — which is
# what the real shared Alloy did before mini-config #47.
if grep -Eq '^[[:space:]]*logs[[:space:]]*=[[:space:]]*\[otelcol\.processor\.transform\.apps\.input\]' "$STANDIN_CODE"; then
  pass "the apps receiver routes logs through the vocabulary translation"
else
  fail "the apps receiver does not route logs through otelcol.processor.transform.apps"
fi
# The shared Alloy (mini-config #47) stamps no marker of its own: the pusher
# marks its data, and `telemetry_source` fills the indexed `log_source`. A
# stand-in that stamped `otlp` itself would let e2e pass on a server that sets
# no marker — the exact fault dev would then show.
if grep -q 'set(attributes\["log_source"\], attributes\["telemetry_source"\]) where attributes\["log_source"\] == nil' "$STANDIN_CODE"; then
  pass "telemetry_source fills log_source where absent, as in the shared Alloy"
else
  fail "the stand-in does not translate telemetry_source into log_source as config.alloy does"
fi
stamped="$(grep -E 'set\((resource\.)?attributes\["(log_source|telemetry_source)"\], "(otlp|docker|file)"\)' "$STANDIN_CODE" || true)"
if [ -z "$stamped" ]; then
  pass "the collector stamps no server-side marker of its own"
else
  fail "the stand-in stamps a marker the pusher should set: $stamped"
fi
if grep -q '"log_source" = "docker"' "$STANDIN_CODE"; then
  pass "the Docker scrape marks its lines log_source=docker"
else
  fail "the Docker scrape does not set log_source=docker"
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "alloy config validation FAILED ($FAILURES)" >&2
  exit 1
fi
echo "alloy config validation OK"
