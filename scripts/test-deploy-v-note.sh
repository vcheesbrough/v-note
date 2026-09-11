#!/bin/sh
# Exercise deploy-v-note.sh's input validation without touching docker.
#
# Iteration 22 made this load-bearing: the deploy script no longer knows dev from
# prod, so every environment-specific value arrives as a parameter from the
# calling Woodpecker step. A missing one is not a crash but a *silently wrong*
# deploy — a prod container labelled env=dev, a deploy into a parallel compose
# project that orphans the real one, an app with no runtime config. The guards
# are the only thing standing in the way, and the pipeline's own deploy step only
# ever exercises the happy path.
#
# A stub `docker` on PATH records every invocation, so each case can assert not
# just "failed" but "failed before touching anything".
#
# POSIX sh, not bash: this runs in the `docker:27-cli` CI image, which ships the
# compose plugin the pre-flight cases need but no bash.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/scripts/deploy-v-note.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

CALLS="$WORK/docker-calls"
REAL_DOCKER="$(command -v docker || true)"
if [ -n "$REAL_DOCKER" ] && ! "$REAL_DOCKER" compose version >/dev/null 2>&1; then
  REAL_DOCKER=""
fi

# Compose's `:?` guards are part of what the script relies on, so `compose config`
# is delegated to the real binary when there is one; everything else is stubbed,
# which keeps `docker login` away from the network even if a guard regresses.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/docker" <<'STUB'
#!/bin/sh
if [ "$1" = "compose" ] && [ "$2" = "config" ] && [ -n "$REAL_DOCKER" ]; then
  exec "$REAL_DOCKER" "$@"
fi
# Record the invocation, plus the compose-facing values the script computes.
echo "$*" >> "$DOCKER_CALLS"
if [ "$1" = "compose" ] && [ "$2" = "up" ]; then
  echo "metrics scrape=$V_NOTE_METRICS_SCRAPE port=$V_NOTE_METRICS_PORT" >> "$DOCKER_CALLS"
  echo "tag=$V_NOTE_IMAGE_TAG version=$APP_VERSION" >> "$DOCKER_CALLS"
fi
exit 0
STUB
chmod +x "$WORK/bin/docker"

# Copy the compose files somewhere with no `.env` beside them. Compose treats the
# first file's directory as the project directory and auto-loads `.env` from it,
# and a developer box has deploy/.env — which would supply the very parameters
# the guard cases below remove, making them pass locally and mean nothing.
mkdir -p "$WORK/deploy"
cp "$ROOT"/deploy/*.yml "$WORK/deploy/"

FAILURES=0
pass() { echo "  ok   — $1"; }
fail() { echo "  FAIL — $1" >&2; FAILURES=$((FAILURES + 1)); }

# Every parameter a healthy deploy-dev step supplies. Each case mutates one.
base_env() {
  export DOCKER_CALLS="$CALLS" REAL_DOCKER
  export PATH="$WORK/bin:$PATH"
  export REGISTRY_USER=user REGISTRY_PASSWORD=pass POSTGRES_PASSWORD=pw
  export SOVEREIGN_CONFIG_ACCESS_URL_FILE=/run/secrets/access-url
  export V_NOTE_METRICS_ADDR=0.0.0.0:9090
  export COMPOSE_PROJECT_NAME=v-note-test
  export COMPOSE_FILE="$WORK/deploy/docker-compose.yml"
  export V_NOTE_IMAGE_REPOS=registry.desync.link/v-note
  export V_NOTE_HOST=v-notes-test.desync.link
  export V_NOTE_CONTAINER_NAME=v-note-test
  export DB_VOLUME=v-note-test-db
  export APP_ENV=test
  export V_NOTE_IMAGE_TAG=9.9.9
}

# Run the deploy script with `$1` applied to the base environment. Echoes the
# exit code; leaves stdout+stderr in $WORK/out and docker calls in $CALLS.
run_deploy() {
  : > "$CALLS"
  local rc=0
  (
    cd "$ROOT"
    base_env
    eval "$1"
    "$SCRIPT"
  ) > "$WORK/out" 2>&1 || rc=$?
  echo "$rc"
}

# The guards exist to stop a bad deploy *before* it does anything, so "exited
# non-zero" is not enough on its own — assert no docker call was made either.
assert_fails_untouched() {
  local desc="$1" mutation="$2" expect_msg="${3:-}"
  local rc
  rc=$(run_deploy "$mutation")
  if [ "$rc" -eq 0 ]; then
    fail "$desc: expected non-zero exit, got 0"
    return
  fi
  if [ -s "$CALLS" ]; then
    fail "$desc: exited $rc but called docker: $(tr '\n' '; ' < "$CALLS")"
    return
  fi
  if [ -n "$expect_msg" ] && ! grep -qF "$expect_msg" "$WORK/out"; then
    fail "$desc: exited $rc with no docker call, but message missing '$expect_msg': $(cat "$WORK/out")"
    return
  fi
  pass "$desc"
}

assert_succeeds() {
  local desc="$1" mutation="$2"
  local rc
  rc=$(run_deploy "$mutation")
  if [ "$rc" -ne 0 ]; then
    fail "$desc: expected success, got $rc: $(cat "$WORK/out")"
    return
  fi
  pass "$desc"
}

assert_recorded() {
  local desc="$1" expected="$2"
  if grep -qF "$expected" "$CALLS"; then
    pass "$desc"
  else
    fail "$desc: '$expected' not among docker calls: $(tr '\n' '; ' < "$CALLS")"
  fi
}

echo "==> parameters guarded by name (each has a silently-wrong default)"
for name in APP_ENV COMPOSE_PROJECT_NAME SOVEREIGN_CONFIG_ACCESS_URL_FILE V_NOTE_IMAGE_REPOS; do
  assert_fails_untouched "$name unset" "unset $name" "ERROR: $name is required"
  assert_fails_untouched "$name blank" "$name=''" "ERROR: $name is required"
done

echo "==> metrics-addr must be host:port or the literal 'disabled'"
assert_fails_untouched "V_NOTE_METRICS_ADDR malformed" "V_NOTE_METRICS_ADDR=nonsense" \
  "must be host:port or 'disabled'"
# Blank is rejected rather than treated as "disabled": it is indistinguishable
# from a broker secret that failed to resolve.
assert_fails_untouched "V_NOTE_METRICS_ADDR blank" "V_NOTE_METRICS_ADDR=''" \
  "must be host:port or 'disabled'"
assert_fails_untouched "V_NOTE_METRICS_ADDR unset" "unset V_NOTE_METRICS_ADDR"

echo "==> parameters compose guards, caught by the pre-flight before any side effect"
# These have no by-name check: deploy/docker-compose.yml carries `:?` guards, and
# the `docker compose config` pre-flight is what fires them *before* the registry
# login and the image pull. Without that pre-flight they would not surface until
# `up`, so "exited with no docker call" is precisely the property under test.
if [ -n "$REAL_DOCKER" ]; then
  for name in V_NOTE_CONTAINER_NAME V_NOTE_HOST DB_VOLUME POSTGRES_PASSWORD; do
    assert_fails_untouched "$name unset (compose :? guard, before login)" "unset $name" \
      "$name"
  done
  assert_fails_untouched "COMPOSE_FILE pointing at a missing file" \
    "COMPOSE_FILE=$WORK/deploy/nope.yml"
else
  echo "  skip — no docker compose available to resolve the model"
fi

echo "==> happy path derives the scrape labels from the one metrics-addr value"
assert_succeeds "listener address deploys" "true"
assert_recorded "scrape labels follow the listener port" "metrics scrape=true port=9090"
assert_recorded "release tag reaches compose as both image tag and APP_VERSION" \
  "tag=9.9.9 version=9.9.9"
assert_recorded "both the compose up and the explicit recreate run" \
  "compose up -d --force-recreate --no-deps v-note"

assert_succeeds "'disabled' deploys with scraping off" "V_NOTE_METRICS_ADDR=disabled"
assert_recorded "'disabled' turns scraping off, keeping a placeholder port" \
  "metrics scrape=false port=9090"

assert_succeeds "a non-default metrics port is carried through" \
  "V_NOTE_METRICS_ADDR=0.0.0.0:9191"
assert_recorded "scrape port tracks the listener, not a hardcoded 9090" \
  "metrics scrape=true port=9191"

echo "==> the script takes no arguments and ignores any"
# Documented consequence of dropping the dev|prod positional: a stray argument is
# not an error, it is ignored, and the environment decides the target.
assert_succeeds "a stray argument is ignored" "set -- prod"

if [ "$FAILURES" -ne 0 ]; then
  echo "deploy-v-note.sh: $FAILURES check(s) failed" >&2
  exit 1
fi
echo "deploy-v-note.sh input validation OK"
