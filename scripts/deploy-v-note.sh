#!/bin/sh
set -eu

# Deploy one v-note environment. Takes no arguments and ignores any: every
# environment-specific value is a parameter supplied by the calling pipeline step
# (.woodpecker/build.yml), so this script has no idea dev and prod exist and no
# branch to keep in sync with them.
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
#                                      which URL is injected selects the environment
#   V_NOTE_METRICS_ADDR                `host:port` or `disabled`
#   COMPOSE_PROJECT_NAME               compose project (read by docker compose itself)
#   COMPOSE_FILE                       `:`-separated compose files (likewise)
#   V_NOTE_IMAGE_REPOS                 space-separated repos to pull at the release tag
#   V_NOTE_HOST, V_NOTE_CONTAINER_NAME, DB_VOLUME, APP_ENV
#                                      passed straight through to compose
# Optional:
#   V_NOTE_IMAGE_TAG                   overrides the tag in .release-tag
#   DOCKER_NETWORK                     network for the health probe (default v-note-net)

# Minimal image used to probe the app's /health from inside the docker network.
# busybox wget speaks HTTPS and exits non-zero on DNS failure or any non-200.
HEALTH_PROBE_IMAGE="alpine:3.21@sha256:48b0309ca019d89d40f670aa1bc06e426dc0931948452e8491e3d65087abc07d"
HEALTH_TIMEOUT_SECONDS=120

# `docker compose up -d` returns as soon as the container is *created*, so an app
# that starts and then immediately exits (bad config, unreachable dependency)
# still looks like a successful deploy — a crash-looping container once reached
# CI as "green". Poll the app's own /health over the compose network and fail the
# deploy if it never serves.
wait_for_health() {
  container="$1"
  network="$2"
  deadline=$(( $(date +%s) + HEALTH_TIMEOUT_SECONDS ))

  echo "==> waiting for $container to serve /health (timeout ${HEALTH_TIMEOUT_SECONDS}s)"
  while :; do
    if docker run --rm --network "$network" "$HEALTH_PROBE_IMAGE" \
        wget --no-check-certificate -q -O /dev/null "https://$container/health" >/dev/null 2>&1; then
      echo "==> $container is healthy"
      return 0
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      echo "ERROR: $container did not serve /health within ${HEALTH_TIMEOUT_SECONDS}s" >&2
      echo "--- docker ps ---" >&2
      docker ps -a --filter "name=$container" --format '{{.Names}}\t{{.Status}}' >&2 || true
      echo "--- last 50 log lines from $container ---" >&2
      docker logs --tail 50 "$container" >&2 2>&1 || true
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
#   APP_ENV                           compose falls back to `dev`, so a prod
#                                     deploy would label itself `env=dev`
#   COMPOSE_PROJECT_NAME              compose falls back to the compose file's
#                                     directory name (`deploy`), deploying into a
#                                     parallel project and orphaning the real one
#   SOVEREIGN_CONFIG_ACCESS_URL_FILE  compose is happy with a blank secret; the
#                                     app then starts with no runtime config at
#                                     all, which is also what a broker secret
#                                     that failed to resolve looks like
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
# This step cannot read sovereign-config itself (it runs in a docker CLI image and
# the connection is gRPC), so the Woodpecker sovereign-config broker supplies it:
# `v_note_{dev,prod}_metrics_addr` is an *alias* of
# `/v-note/{dev,prod}/server/observability/metrics-addr`, one stored value at two
# canonical paths. The container is not given an env override — the app reads that
# same leaf directly through its own sovereign-config client.
case "$V_NOTE_METRICS_ADDR" in
  # The app also treats an empty metrics-addr as disabled, but an empty value
  # here is indistinguishable from a secret that failed to resolve, so it falls
  # through to the error below — spell it `disabled` in sovereign-config.
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
# keeps serving stale database/OIDC/App Links values, and the health probe below
# would pass against it, reporting a green deploy that changed nothing.
#
# Before iteration 19 config was compose env, so a config change moved the model
# and forced a recreate; that coupling is gone. Recreate the app explicitly. It is
# stateless so this is cheap, and `--no-deps` leaves postgres untouched.
docker compose up -d --force-recreate --no-deps v-note

wait_for_health "$V_NOTE_CONTAINER_NAME" "${DOCKER_NETWORK:-v-note-net}"
