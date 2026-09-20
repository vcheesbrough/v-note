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
