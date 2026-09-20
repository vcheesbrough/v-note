#!/bin/sh
set -eu

# Deploy one v-note environment. Takes no arguments and ignores any: every
# environment-specific value is a parameter supplied by the calling pipeline step
# (.woodpecker/deploy.yml), so this script has no idea which environments exist
# and no branch to keep in sync with them.
#
# Since #391 the parameters arrive from two places, and this script cannot tell
# them apart — which is the point. The step sets the environment's identity
# directly, and wraps this script in `sovereign-config render
# /v-note/devops/<env>/compose`, which puts that layer's leaves in the
# environment before exec'ing it. A missing value therefore fails the same way
# whichever side it should have come from, and the guards below are unchanged.
#
# What stays here is the shell that is awkward to inline: the metrics-label
# derivation (one value, two consumers, so it cannot be passed in pre-split
# without reintroducing drift), the recreate step and its rationale, the health
# gate, and a guard on the handful of parameters that would otherwise fail
# silently.
#
# Required parameters:
#   REGISTRY_USER / REGISTRY_PASSWORD  registry.desync.link credentials
#   POSTGRES_PASSWORD                  consumed directly by the postgres service
#   SOVEREIGN_CONFIG_ACCESS_URL_FILE   unlocks the app's sovereign-config subtree;
#                                      which URL is injected selects the environment.
#                                      Not SOVEREIGN_CONFIG_URL — that is the
#                                      deploy step's own credential for the
#                                      /v-note/devops layer, and `render` strips it
#                                      before this script starts
#   V_NOTE_METRICS_ADDR                `host:port` or `disabled`
#   COMPOSE_PROJECT_NAME               compose project (read by docker compose itself)
#   COMPOSE_FILE                       `:`-separated compose files (likewise)
#   V_NOTE_IMAGE_REPOS                 space-separated repos to pull at the release tag
#   V_NOTE_HOST, V_NOTE_CONTAINER_NAME, DB_VOLUME, APP_ENV
#                                      passed straight through to compose
# Optional:
#   V_NOTE_IMAGE_TAG                   overrides the tag in .release-tag
#   DOCKER_NETWORK                     compose network name (default v-note-net)
#   COMPOSE_PROFILES                   naming `sqltool` enables the SQL console (#299)
# Required only when COMPOSE_PROFILES names `sqltool`:
#   PGWEB_DB_PASSWORD                  password for the read-only v_note_pgweb role
#   PGWEB_AUTH_USER / PGWEB_AUTH_PASS  the console's basic-auth backstop; this
#                                      script derives PGWEB_AUTH_B64 from them for
#                                      the Traefik header that satisfies it

HEALTH_TIMEOUT_SECONDS=120

# Everything the old external probe could not say. .State.Health.Log holds the
# last five attempts with their exit code and captured output — curl's own error
# text, which is usually the whole answer — and the old `wget -q -O /dev/null
# 2>&1` threw all of it away.
health_diagnostics() {
  container="$1"
  echo "--- docker ps ---" >&2
  docker ps -a --filter "name=$container" --format '{{.Names}}\t{{.Status}}' >&2 || true
  echo "--- last health probe attempts ---" >&2
  docker inspect -f '{{range .State.Health.Log}}exit={{.ExitCode}} {{.Output}}{{end}}' \
    "$container" >&2 2>/dev/null || true
  echo "--- last 50 log lines from $container ---" >&2
  docker logs --tail 50 "$container" >&2 2>&1 || true
}

# `docker compose up -d` returns as soon as the container is *created*, so an app
# that starts and then immediately exits (bad config, unreachable dependency)
# still looks like a successful deploy — a crash-looping container once reached
# CI as "green".
#
# The gate is the container's own healthcheck, baked into the image by
# Dockerfile.web, rather than a second probe run from out here. One definition of
# healthy: this passes on exactly the condition `docker ps` reports to whoever
# looks next. It also means both failures are decided rather than waited out —
# a dead process is caught by its run state, and `unhealthy` is terminal because
# docker has already applied the image's configured retries. The deadline below
# is now only for an app that stays up and never finishes starting.
wait_for_health() {
  container="$1"
  deadline=$(( $(date +%s) + HEALTH_TIMEOUT_SECONDS ))

  echo "==> waiting for $container to report healthy (timeout ${HEALTH_TIMEOUT_SECONDS}s)"
  while :; do
    # Run state and health in one call, because the two failures look nothing
    # alike. A container whose process dies on bad config never reports
    # unhealthy — it exits (or, under `restart: unless-stopped`, loops through
    # `restarting`) while its health status sits at whatever it last was. That
    # is the crash-loop this gate exists for, and watching run state is what
    # catches it in seconds rather than at the deadline.
    state=$(docker inspect \
      -f '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' \
      "$container" 2>/dev/null || echo "missing missing")
    run_state=${state%% *}
    status=${state#* }

    case "$run_state" in
      exited|dead|restarting)
        echo "ERROR: $container is $run_state — it did not stay up long enough to be healthy" >&2
        health_diagnostics "$container"
        return 1
        ;;
      missing)
        # compose created it moments ago; absent now means removed under us.
        echo "ERROR: $container does not exist" >&2
        return 1
        ;;
    esac

    case "$status" in
      healthy)
        echo "==> $container is healthy"
        return 0
        ;;
      unhealthy)
        echo "ERROR: $container reported unhealthy" >&2
        health_diagnostics "$container"
        return 1
        ;;
      none)
        # Rolling back to a tag built before the image carried a HEALTHCHECK.
        # Say so rather than polling a status that will never arrive.
        echo "ERROR: $container has no healthcheck — the deployed image predates" >&2
        echo "       the HEALTHCHECK in Dockerfile.web, so there is nothing to gate on" >&2
        return 1
        ;;
    esac
    if [ "$(date +%s)" -ge "$deadline" ]; then
      echo "ERROR: $container did not report healthy within ${HEALTH_TIMEOUT_SECONDS}s (last status: $run_state/$status)" >&2
      health_diagnostics "$container"
      return 1
    fi
    sleep 3
  done
}

require_env() {
  name="$1"
  if [ -z "$(printenv "$name")" ]; then
    echo "ERROR: $name is required" >&2
    exit 1
  fi
}

# Only the parameters that would otherwise fail *silently* are checked here.
# Everything else already fails loudly on its own and a second check would just
# rot: `set -u` catches any unset variable this script dereferences;
# POSTGRES_PASSWORD, V_NOTE_HOST, V_NOTE_CONTAINER_NAME and DB_VOLUME carry `:?`
# guards in deploy/docker-compose.yml, fired early by the `compose config`
# pre-flight below along with a missing or wrong COMPOSE_FILE; empty registry
# credentials fail `docker login`; and an empty V_NOTE_METRICS_ADDR is rejected by
# the case below.
#
# These four are different — each has a default that is silently wrong:
#   APP_ENV                           compose falls back to `dev`, so any other
#                                     environment would mislabel itself `env=dev`
#   COMPOSE_PROJECT_NAME              compose falls back to the compose file's
#                                     directory name (`deploy`), deploying into a
#                                     parallel project and orphaning the real one
#   SOVEREIGN_CONFIG_ACCESS_URL_FILE  compose is happy with a blank secret; the
#                                     app then starts with no runtime config at
#                                     all, which is also what a parameter that
#                                     failed to resolve looks like
#   V_NOTE_IMAGE_REPOS                the pull loop below just does nothing
for name in APP_ENV COMPOSE_PROJECT_NAME SOVEREIGN_CONFIG_ACCESS_URL_FILE V_NOTE_IMAGE_REPOS; do
  require_env "$name"
done

# --- SQL console (#299) -----------------------------------------------------
#
# Active only when COMPOSE_PROFILES names `sqltool`. Everything in this block is
# conditional on that, because the console is an opt-in extra and an environment
# that does not run it must not need its credentials.
#
# The guards are HERE rather than as `:?` in deploy/docker-compose.yml on
# purpose: compose interpolates the whole file before filtering by profile, so a
# `:?` on a profiled service fires even when that service is switched off, which
# would make these credentials mandatory for every deploy. This is the same
# loud failure, scoped to when it actually means something.
#
# What must never happen is a *fallback*. `${PGWEB_DB_PASSWORD:-$POSTGRES_PASSWORD}`
# or any equivalent silently connects the console as the application's superuser,
# and pgweb's read-only mode does not stop a superuser reading host files
# through `pg_read_file`. Missing means fail, never substitute.
# Matched the way compose matches it: an exact name in a comma-separated list,
# not a substring. `nosqltool` or `sqltool-preview` would otherwise take this
# branch, demand three credentials, and then run `up -d … sqltool` for a service
# compose never activated — failing a deploy that had nothing to do with the
# console. This variable is a documented operator knob, so it is worth parsing
# it correctly rather than approximately.
case ",${COMPOSE_PROFILES:-}," in
  *,sqltool,*)
    SQLTOOL_ENABLED=1
    for name in PGWEB_DB_PASSWORD PGWEB_AUTH_USER PGWEB_AUTH_PASS; do
      require_env "$name"
    done
    # One credential, two consumers — pgweb checks it and Traefik injects it —
    # so it is derived here rather than stored twice. Exactly the reasoning
    # behind the metrics-label derivation below.
    PGWEB_AUTH_B64=$(printf '%s:%s' "$PGWEB_AUTH_USER" "$PGWEB_AUTH_PASS" | base64 | tr -d '\n')
    export PGWEB_AUTH_B64
    ;;
  *)
    SQLTOOL_ENABLED=0
    # Compose still interpolates the profiled service, so these must resolve to
    # *something*. Empty is correct: the service is not created, and an empty
    # value is what an unset variable would produce anyway.
    export PGWEB_DB_PASSWORD="${PGWEB_DB_PASSWORD:-}"
    export PGWEB_AUTH_USER="${PGWEB_AUTH_USER:-}"
    export PGWEB_AUTH_PASS="${PGWEB_AUTH_PASS:-}"
    export PGWEB_AUTH_B64=""
    ;;
esac

# Alloy's scrape-discovery labels are derived from the metrics listener address,
# so the listener and the thing scraping it cannot disagree — previously the
# labels hardcoded `scrape=true` and port 9090 while `observability/metrics-addr`
# was freely configurable, so moving or disabling the listener silently lost
# metrics. Derived here rather than passed in pre-split precisely so there is one
# value: two parameters could drift from each other and from the listener.
#
# `sovereign-config render` supplies it (#391):
# `/v-note/devops/<env>/compose/V_NOTE_METRICS_ADDR` is an *alias* of
# `/v-note/<env>/server/observability/metrics-addr`, one stored value at two
# canonical paths. The container is not given an env override — the app reads that
# same leaf directly through its own sovereign-config client.
case "$V_NOTE_METRICS_ADDR" in
  # The app also treats an empty metrics-addr as disabled, but an empty value
  # here is indistinguishable from a parameter that failed to resolve, so it
  # falls through to the error below — spell it `disabled` in sovereign-config.
  disabled)
    V_NOTE_METRICS_SCRAPE="false"
    V_NOTE_METRICS_PORT="9090"
    ;;
  *:*)
    V_NOTE_METRICS_SCRAPE="true"
    V_NOTE_METRICS_PORT="${V_NOTE_METRICS_ADDR##*:}"
    ;;
  *)
    echo "ERROR: V_NOTE_METRICS_ADDR must be host:port or 'disabled' (got: '$V_NOTE_METRICS_ADDR')" >&2
    exit 1
    ;;
esac

# --- client telemetry sidecar (#354) ----------------------------------------
#
# The sidecar's Alloy config reaches its container as a compose `configs:` entry
# sourced from this variable, not as a bind mount: this script runs inside a CI
# container against the host's docker socket, so a bind-mount path would be
# resolved on the host, where this checkout does not exist.
#
# Read here rather than passed in by the pipeline step so that there is nothing
# for a caller to forget — the file is in the repo, next to the compose file that
# consumes it, and is the same for every environment.
#
# Assigned and exported on separate lines deliberately. `export X="$(cat f)"`
# returns export's status, not cat's, so a missing file would slip past `set -e`
# and hand compose an empty string — which it accepts, producing a sidecar that
# starts, reports ready and accepts nothing.
CLIENT_TELEMETRY_ALLOY_CONFIG=$(cat deploy/alloy/client-telemetry.alloy)
export CLIENT_TELEMETRY_ALLOY_CONFIG

# The remaining parameters are enforced by compose's own `:?` guards, which would
# otherwise not fire until `up` — after a registry login and a pull. Resolving the
# model first is client-side only (no daemon, no network), so every parameter is
# now checked before anything with a side effect happens. It also catches an unset
# or wrong COMPOSE_FILE and any schema error in the compose files themselves.
docker compose config --quiet

# Every input is validated above this line; everything below has side effects.
release_tag="${V_NOTE_IMAGE_TAG:-$(cat .release-tag)}"

echo "$REGISTRY_PASSWORD" | docker login registry.desync.link -u "$REGISTRY_USER" --password-stdin

docker volume create "$DB_VOLUME"
for repo in $V_NOTE_IMAGE_REPOS; do
  docker pull "$repo:$release_tag"
done

# Everything else compose needs is already in the environment, passed in by the
# pipeline step. These four are the only values this script computes.
export V_NOTE_IMAGE_TAG="$release_tag"
export APP_VERSION="$release_tag"
export V_NOTE_METRICS_PORT
export V_NOTE_METRICS_SCRAPE

docker compose up -d

# The server snapshots sovereign-config once at startup, and that config lives
# *outside* the compose model — so changing a leaf, or rotating the access URL,
# produces no model change and the `up -d` above is a no-op. The old container
# keeps serving stale database/OIDC/App Links values, and the health gate below
# would pass against it — it is still healthy — reporting a green deploy that
# changed nothing.
#
# Before iteration 19 config was compose env, so a config change moved the model
# and forced a recreate; that coupling is gone. Recreate the app explicitly. It is
# stateless so this is cheap, and `--no-deps` leaves postgres untouched.
docker compose up -d --force-recreate --no-deps v-note

wait_for_health "$V_NOTE_CONTAINER_NAME"

# --- SQL console provisioning (#299) ----------------------------------------
#
# Deliberately after the health gate: the migration that creates `v_note_pgweb`
# and its grants runs at app startup, so the role does not exist until the app
# is up. Only the password is set here — the role, its grants and its per-role
# settings (read-only, timeouts, statement logging) are all in the migration, so
# they reach every environment including the e2e stack rather than only the ones
# this script deploys.
#
# Idempotent: re-setting the same password on every deploy is a no-op, and it is
# what makes rotating the credential a matter of changing the stored leaf rather
# than a manual visit to the database.
if [ "$SQLTOOL_ENABLED" = "1" ]; then
  echo "==> provisioning the v_note_pgweb login"
  # The password travels as an env var on the exec rather than inside the SQL
  # text, and psql's \getenv keeps it out of the statement this script writes.
  # PGPASSWORD is read from the postgres container's own environment, so the
  # app's credential is never handled out here at all.
  #
  # Nothing in this block may echo: these values do not come from `from_secret`
  # since #391, so Woodpecker does not mask them in the step log.
  docker compose exec -T \
    -e PGWEB_NEW_PASSWORD="$PGWEB_DB_PASSWORD" \
    postgres \
    sh -ec 'PGPASSWORD="$POSTGRES_PASSWORD" psql -v ON_ERROR_STOP=1 -q \
      -h 127.0.0.1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" <<'\''SQL'\''
\getenv pgweb_pw PGWEB_NEW_PASSWORD
ALTER ROLE v_note_pgweb WITH LOGIN PASSWORD :'\''pgweb_pw'\'';
SQL'

  # The console came up before the role could log in, so it has been failing its
  # connection since `up`. Recreate it now that the credential works — the same
  # reasoning as the app recreate above, for the same reason (nothing in the
  # compose model changed, so `up -d` alone would be a no-op).
  echo "==> recreating the SQL console against the provisioned role"
  docker compose up -d --force-recreate --no-deps sqltool
else
  # Turning the toggle off has to actually turn it off, and nothing else here
  # does that. `docker compose up -d` leaves a running container for a service
  # that has dropped out of the active profile, and — verified against this
  # compose version — `--remove-orphans` does NOT reap it either: a profiled
  # service is still *defined* in the file, so compose does not consider it an
  # orphan. Without this line the console would keep serving after being
  # switched off, which is the failure mode that matters most for this service.
  #
  # `--profile sqltool` is required to address a service outside the active
  # profile at all. `rm -sf` stops and removes, and exits 0 when there is
  # nothing to remove, so this is a no-op on every deploy that never had one.
  echo "==> ensuring the SQL console is not running"
  docker compose --profile sqltool rm -sf sqltool
fi
