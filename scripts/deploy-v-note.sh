#!/bin/sh
set -eu

# Minimal image used to probe the app's /health from inside the docker network.
# busybox wget speaks HTTPS and exits non-zero on DNS failure or any non-200.
HEALTH_PROBE_IMAGE="alpine:3.21@sha256:48b0309ca019d89d40f670aa1bc06e426dc0931948452e8491e3d65087abc07d"
HEALTH_TIMEOUT_SECONDS=120

usage() {
  echo "Usage: $0 dev|prod" >&2
}

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

target="${1:-}"
if [ "$target" != "dev" ] && [ "$target" != "prod" ]; then
  usage
  exit 1
fi

# Runtime app config (database/oidc/observability/android) now comes from
# sovereign-config; the only app secret this script handles is the access URL that
# unlocks it. It arrives as SOVEREIGN_CONFIG_ACCESS_URL_FILE (Woodpecker secret) and
# compose turns it into the docker secret of the same name — see deploy/docker-compose.yml.
# POSTGRES_PASSWORD stays here because the postgres service consumes it directly
# (same value lives in both OpenBao and sovereign-config — see card #273).
for name in REGISTRY_USER REGISTRY_PASSWORD POSTGRES_PASSWORD SOVEREIGN_CONFIG_ACCESS_URL_FILE; do
  require_env "$name"
done

if [ -n "${V_NOTE_IMAGE_TAG:-}" ]; then
  release_tag="$V_NOTE_IMAGE_TAG"
else
  release_tag="$(cat .release-tag)"
fi

echo "$REGISTRY_PASSWORD" | docker login registry.desync.link -u "$REGISTRY_USER" --password-stdin

case "$target" in
  dev)
    project="v-note-dev"
    db_volume="v-note-dev-db"
    # Canonical environment name, matching the sovereign-config
    # `observability/environment` leaf the server uses for the OTEL
    # `deployment.environment` trace attribute. Previously this was the branch
    # name, which split one deployment across two values: traces said `dev` while
    # the metrics/log discovery labels said e.g. `feat/foo`, so no single
    # environment filter matched all three signals. The build is already
    # identified by `observability.release` (the release tag) and the image's
    # `org.opencontainers.image.revision`, so the branch is not needed here.
    app_env="dev"
    v_note_host="v-notes-dev.desync.link"
    v_note_container_name="v-note-dev"
    compose_files="-f deploy/docker-compose.yml -f deploy/docker-compose.android-apk.yml"
    docker pull "registry.desync.link/v-note-android:$release_tag"
    ;;
  prod)
    project="v-note"
    db_volume="v-note-prod-db"
    app_env="production"
    v_note_host="v-notes.desync.link"
    v_note_container_name="v-note"
    compose_files="-f deploy/docker-compose.yml"
    ;;
esac

docker volume create "$db_volume"
docker pull "registry.desync.link/v-note:$release_tag"

# Single source for the metrics listener in a deployment. The app override and
# Alloy's scrape-discovery labels are both derived from it, so the listener and
# the thing scraping it cannot disagree — previously the labels hardcoded
# `scrape=true` and port 9090 while `observability/metrics-addr` was freely
# configurable, so moving or disabling the listener silently lost metrics.
#
# This step cannot read sovereign-config itself (it runs in a docker CLI image and
# the connection is gRPC), so the value lives here for now. When the Woodpecker
# sovereign-config broker lands it will supply this from
# `observability/metrics-addr`, making it literally the same value in both places.
metrics_addr="${V_NOTE_METRICS_ADDR:-0.0.0.0:9090}"
case "$metrics_addr" in
  disabled|"")
    metrics_scrape="false"
    metrics_port="9090"
    ;;
  *:*)
    metrics_scrape="true"
    metrics_port="${metrics_addr##*:}"
    ;;
  *)
    echo "ERROR: V_NOTE_METRICS_ADDR must be host:port or 'disabled' (got: '$metrics_addr')" >&2
    exit 1
    ;;
esac

V_NOTE_IMAGE_TAG="$release_tag" \
APP_VERSION="$release_tag" \
APP_ENV="$app_env" \
VNOTE_PROTOCOL_VERSION="${VNOTE_PROTOCOL_VERSION:-2}" \
V_NOTE_HOST="$v_note_host" \
V_NOTE_CONTAINER_NAME="$v_note_container_name" \
DB_VOLUME="$db_volume" \
POSTGRES_PASSWORD="$POSTGRES_PASSWORD" \
SOVEREIGN_CONFIG_ACCESS_URL_FILE="$SOVEREIGN_CONFIG_ACCESS_URL_FILE" \
VNOTE__OBSERVABILITY__METRICS_ADDR="$metrics_addr" \
V_NOTE_METRICS_PORT="$metrics_port" \
V_NOTE_METRICS_SCRAPE="$metrics_scrape" \
docker compose -p "$project" $compose_files up -d

wait_for_health "$v_note_container_name" "${DOCKER_NETWORK:-v-note-net}"
