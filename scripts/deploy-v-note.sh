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

for name in REGISTRY_USER REGISTRY_PASSWORD OIDC_CLIENT_SECRET POSTGRES_PASSWORD ASSETLINKS_JSON; do
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
    oidc_issuer_url="https://auth.desync.link/application/o/v-note-dev/"
    oidc_client_id="v-note-browser-dev"
    oidc_redirect_uri="https://v-notes-dev.desync.link/auth/callback"
    required_scope="v-note:dev:access"
    oidc_end_session_url="https://auth.desync.link/application/o/v-note-dev/end-session/"
    oidc_android_client_id="v-note-android-dev"
    oidc_android_issuer_url="https://auth.desync.link/application/o/v-note-android-dev/"
    compose_files="-f deploy/docker-compose.yml -f deploy/docker-compose.android-apk.yml"
    docker pull "registry.desync.link/v-note-android:$release_tag"
    ;;
  prod)
    project="v-note"
    db_volume="v-note-prod-db"
    app_env="production"
    v_note_host="v-notes.desync.link"
    v_note_container_name="v-note"
    oidc_issuer_url="https://auth.desync.link/application/o/v-note-prod/"
    oidc_client_id="v-note-browser-prod"
    oidc_redirect_uri="https://v-notes.desync.link/auth/callback"
    required_scope="v-note:prod:access"
    oidc_end_session_url="https://auth.desync.link/application/o/v-note-prod/end-session/"
    oidc_android_client_id="v-note-android-prod"
    oidc_android_issuer_url="https://auth.desync.link/application/o/v-note-android-prod/"
    compose_files="-f deploy/docker-compose.yml"
    ;;
esac

docker volume create "$db_volume"
docker pull "registry.desync.link/v-note:$release_tag"

V_NOTE_IMAGE_TAG="$release_tag" \
APP_VERSION="$release_tag" \
APP_ENV="$app_env" \
OTEL_EXPORTER_OTLP_ENDPOINT="${OTEL_EXPORTER_OTLP_ENDPOINT:-http://monitor-alloy:4317}" \
OTEL_EXPORTER_OTLP_PROTOCOL="${OTEL_EXPORTER_OTLP_PROTOCOL:-grpc}" \
OTEL_SERVICE_NAME="${OTEL_SERVICE_NAME:-v-note}" \
VNOTE_PROTOCOL_VERSION="${VNOTE_PROTOCOL_VERSION:-1}" \
V_NOTE_HOST="$v_note_host" \
V_NOTE_CONTAINER_NAME="$v_note_container_name" \
DB_VOLUME="$db_volume" \
POSTGRES_PASSWORD="$POSTGRES_PASSWORD" \
OIDC_ISSUER_URL="$oidc_issuer_url" \
OIDC_CLIENT_ID="$oidc_client_id" \
OIDC_CLIENT_SECRET="$OIDC_CLIENT_SECRET" \
OIDC_REDIRECT_URI="$oidc_redirect_uri" \
REQUIRED_SCOPE="$required_scope" \
OIDC_END_SESSION_URL="$oidc_end_session_url" \
OIDC_ANDROID_CLIENT_ID="$oidc_android_client_id" \
OIDC_ANDROID_ISSUER_URL="$oidc_android_issuer_url" \
ASSETLINKS_JSON="$ASSETLINKS_JSON" \
docker compose -p "$project" $compose_files up -d
