#!/bin/sh
# Exercise deploy-v-note.sh's input validation without touching docker.
#
# Iteration 22 made this load-bearing: the deploy script no longer knows which
# environment it is deploying, so every environment-specific value arrives as a
# parameter from the calling Woodpecker step. A missing one is not a crash but a
# *silently wrong* deploy — a container labelled with the wrong env, a deploy
# into a parallel compose project that orphans the real one, an app with no
# runtime config. The guards are the only thing standing in the way, and the
# pipeline's own deploy step only ever exercises the happy path.
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
  # #299: the Traefik header the script derives, so the test can assert it
  # matches the credential pgweb is configured to check.
  echo "pgweb-auth-b64=${PGWEB_AUTH_B64:-}" >> "$DOCKER_CALLS"
fi
# The health gate reads the container's own state, so the stub has to answer
# `docker inspect` — a stub that just exits 0 would return an empty status and
# leave every happy-path case spinning until the 120s deadline.
# STUB_HEALTH_SEQUENCE is `<run-state>:<health>` per poll, the last entry
# repeating, which is what lets the cases below drive `starting` -> `healthy`,
# `unhealthy`, a crash-looping container, and an image with no healthcheck.
if [ "$1" = "inspect" ]; then
  case "$*" in
    *State.Health.Log*)
      echo "exit=1 curl: (7) Failed to connect to 127.0.0.1 port 443"
      exit 0
      ;;
  esac
  index=$(cat "$STUB_HEALTH_INDEX" 2>/dev/null || echo 1)
  echo $(( index + 1 )) > "$STUB_HEALTH_INDEX"
  entry=$(echo "$STUB_HEALTH_SEQUENCE" | cut -d' ' -f"$index")
  [ -n "$entry" ] || entry=$(echo "$STUB_HEALTH_SEQUENCE" | awk '{print $NF}')
  echo "${entry%%:*} ${entry#*:}"
  exit 0
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
  export STUB_HEALTH_INDEX="$WORK/health-index"
  export STUB_HEALTH_SEQUENCE=running:healthy
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
  rm -f "$WORK/health-index"
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

# The health gate runs *after* the deploy has done its work, so unlike the guard
# cases these assert on the exit code and the message, not on docker being
# untouched.
assert_gate_fails() {
  local desc="$1" mutation="$2" expect_msg="$3"
  local rc
  rc=$(run_deploy "$mutation")
  if [ "$rc" -eq 0 ]; then
    fail "$desc: expected non-zero exit, got 0"
    return
  fi
  if ! grep -qF "$expect_msg" "$WORK/out"; then
    fail "$desc: exited $rc but message missing '$expect_msg': $(cat "$WORK/out")"
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
# from a parameter that failed to resolve.
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

echo "==> the health gate reads the container's own healthcheck status"
# Iteration 23 replaced an external wget probe with the container's own status.
# The happy-path cases above already cover "healthy immediately"; these cover the
# outcomes the old probe could not distinguish at all — it saw only "did not
# answer yet" and burned the full timeout on every one of them.
assert_succeeds "a container still starting is waited out" \
  "STUB_HEALTH_SEQUENCE='running:starting running:healthy'"
assert_gate_fails "an unhealthy container fails the deploy" \
  "STUB_HEALTH_SEQUENCE=running:unhealthy" "reported unhealthy"
assert_gate_fails "an unhealthy container dumps the probe attempts" \
  "STUB_HEALTH_SEQUENCE=running:unhealthy" "last health probe attempts"
# The crash loop the gate exists for: the process dies on bad config, so the
# container never reports unhealthy at all — its health status stays wherever it
# was while docker restarts it. Run state is the only thing that shows this.
assert_gate_fails "a crash-looping container fails the deploy" \
  "STUB_HEALTH_SEQUENCE=restarting:starting" "did not stay up long enough"
assert_gate_fails "an exited container fails the deploy" \
  "STUB_HEALTH_SEQUENCE=exited:starting" "did not stay up long enough"
# Rolling back to an image tag built before the HEALTHCHECK existed: there is no
# status to wait for, so say so instead of polling until the deadline.
assert_gate_fails "an image with no healthcheck fails the deploy" \
  "STUB_HEALTH_SEQUENCE=running:none" "has no healthcheck"

echo "==> the SQL console is off unless COMPOSE_PROFILES asks for it (#299)"
# The base environment has no COMPOSE_PROFILES, which is the "no console" case
# every environment that does not want one is in. The console's credentials must
# not be required there — and the disable path must actively run, because
# `docker compose up -d` leaves a running container for a service that dropped
# out of the profile, and `--remove-orphans` does not reap it either (a profiled
# service is still *defined*, so compose does not consider it an orphan).
assert_succeeds "no profile deploys without any PGWEB_* value" "true"
assert_recorded "the console is explicitly torn down when the profile is off" \
  "compose --profile sqltool rm -sf sqltool"
if grep -qF 'compose up -d --force-recreate --no-deps sqltool' "$CALLS"; then
  fail "the console was recreated even though the profile is off"
else
  pass "no console is created when the profile is off"
fi

echo "==> with the profile on, every console credential is required"
# 🚫 The failure this guards against is not a crash, it is a *fallback*. A
# `${PGWEB_DB_PASSWORD:-$POSTGRES_PASSWORD}` written to be helpful connects the
# console as the application's superuser, and pgweb's read-only mode does not
# stop a superuser reading host files through pg_read_file. Missing must mean
# fail, never substitute.
#
# The guards live in this script rather than as compose `:?` because compose
# interpolates the whole file before filtering by profile, so a `:?` on a
# profiled service fires even when the console is switched off.
# `export`, not plain assignment: these names are not in base_env, and the
# script under test is a child process, so an unexported variable would leave
# every case below silently exercising the profile-off path instead.
sqltool_env="export COMPOSE_PROFILES=sqltool PGWEB_DB_PASSWORD=dbpw PGWEB_AUTH_USER=console PGWEB_AUTH_PASS=authpw"
for name in PGWEB_DB_PASSWORD PGWEB_AUTH_USER PGWEB_AUTH_PASS; do
  assert_fails_untouched "$name unset with the profile on" \
    "$sqltool_env; unset $name" "ERROR: $name is required"
  assert_fails_untouched "$name blank with the profile on" \
    "$sqltool_env; $name=''" "ERROR: $name is required"
done

# The console must never be handed the app's credential, whatever else is set.
assert_fails_untouched "POSTGRES_PASSWORD is not a fallback for PGWEB_DB_PASSWORD" \
  "$sqltool_env; unset PGWEB_DB_PASSWORD; POSTGRES_PASSWORD=superuser-pw" \
  "ERROR: PGWEB_DB_PASSWORD is required"

echo "==> with the profile on, the console is provisioned and recreated"
assert_succeeds "a fully-specified console deploys" "$sqltool_env"
assert_recorded "the read-only role's login is provisioned after the health gate" \
  "compose exec -T -e PGWEB_NEW_PASSWORD=dbpw postgres"
assert_recorded "the console is recreated once the credential works" \
  "compose up -d --force-recreate --no-deps sqltool"
if grep -qF 'compose --profile sqltool rm -sf sqltool' "$CALLS"; then
  fail "the console was torn down even though the profile is on"
else
  pass "the console is not torn down when the profile is on"
fi

# One credential, two consumers: pgweb checks it and Traefik injects it. Derived
# rather than stored twice so they cannot drift apart.
expected_b64=$(printf '%s:%s' console authpw | base64 | tr -d '\n')
assert_recorded "the Traefik header is derived from the same user/pass pgweb checks" \
  "pgweb-auth-b64=$expected_b64"

echo "==> COMPOSE_PROFILES is matched as compose matches it, not as a substring"
# A near-miss name must NOT enable the console: taking that branch would demand
# three credentials and then recreate a service compose has not activated,
# failing a deploy that had nothing to do with the console.
assert_succeeds "a near-miss profile name does not enable the console" \
  "export COMPOSE_PROFILES=nosqltool"
assert_recorded "a near-miss name still runs the teardown" \
  "compose --profile sqltool rm -sf sqltool"

assert_succeeds "a profile name with a suffix does not enable the console" \
  "export COMPOSE_PROFILES=sqltool-preview"
if grep -qF 'compose up -d --force-recreate --no-deps sqltool' "$CALLS"; then
  fail "sqltool-preview enabled the console"
else
  pass "sqltool-preview does not enable the console"
fi

# ...and a real multi-profile list must still enable it.
assert_succeeds "sqltool among several profiles enables the console" \
  "$sqltool_env; export COMPOSE_PROFILES=other,sqltool,third"
assert_recorded "a multi-profile list still provisions the role" \
  "compose exec -T -e PGWEB_NEW_PASSWORD=dbpw postgres"

echo "==> the script takes no arguments and ignores any"
# Documented consequence of dropping the <env> positional: a stray argument is
# not an error, it is ignored, and the environment block decides the target.
assert_succeeds "a stray argument is ignored" "set -- some-argument"

echo "==> the script must not print its own environment (#391)"
# Since #391 the deploy's configuration is rendered from sovereign-config rather
# than brokered in, and Woodpecker masks only `from_secret` values — so
# POSTGRES_PASSWORD and the app's access URL now reach this script *unmasked*.
# A leaked credential in a CI log is not self-announcing and is not undone by
# reverting the commit that leaked it, so the two ways this script could print
# them are asserted here rather than left to a comment.
#
# These are source assertions, not behavioural ones: they are cheap, they run in
# the same step as everything above, and the property they protect has no other
# gate.
if grep -q -- 'docker compose config --quiet' "$SCRIPT"; then
  pass "compose config keeps --quiet (it would otherwise dump the resolved model)"
else
  fail "deploy-v-note.sh must keep 'docker compose config --quiet': POSTGRES_PASSWORD is no longer a masked Woodpecker secret (#391)"
fi
# `if ! grep`, not `grep && fail`: this file runs under `set -e`, so a compound
# whose left side fails would exit the suite on the *passing* case.
if ! grep -qE '^[[:space:]]*set[[:space:]]+-[a-z]*x' "$SCRIPT"; then
  pass "the script does not trace its own execution"
else
  fail "deploy-v-note.sh must not run under 'set -x': its environment holds unmasked secrets (#391)"
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "deploy-v-note.sh: $FAILURES check(s) failed" >&2
  exit 1
fi
echo "deploy-v-note.sh input validation OK"
