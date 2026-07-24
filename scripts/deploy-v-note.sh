#!/bin/sh
set -eu

usage() {
  echo "Usage: $0 dev|prod" >&2
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
    app_env="${CI_COMMIT_BRANCH:-dev}"
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

V_NOTE_IMAGE_TAG="$release_tag" \
APP_VERSION="$release_tag" \
APP_ENV="$app_env" \
VNOTE_PROTOCOL_VERSION="${VNOTE_PROTOCOL_VERSION:-2}" \
V_NOTE_HOST="$v_note_host" \
V_NOTE_CONTAINER_NAME="$v_note_container_name" \
DB_VOLUME="$db_volume" \
POSTGRES_PASSWORD="$POSTGRES_PASSWORD" \
SOVEREIGN_CONFIG_ACCESS_URL_FILE="$SOVEREIGN_CONFIG_ACCESS_URL_FILE" \
docker compose -p "$project" $compose_files up -d
