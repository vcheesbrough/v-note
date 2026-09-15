#!/bin/sh
# CI `rust-test` step and `just rust-ci test`: the Rust test suite, including the
# `postgres-tests` feature, against a throwaway Postgres.
#
# The tests run inside `docker build` (Dockerfile.rust-ci --target test) so they
# share build-web's warm crate cache, and a BuildKit RUN step cannot see a
# Woodpecker service or a compose network by name. So this starts Postgres as a
# sibling container on the daemon's default bridge and hands the build its bridge
# IP: a default-network build step reaches it with no host port published and no
# `--network=host` entitlement.
#
# Needs the docker CLI with a daemon, and GITHUB_TOKEN: the private
# sovereign-config dependency is fetched through a BuildKit secret.
set -eu

cd "$(dirname "$0")/.."

# Keep in step with the postgres image pinned in e2e/docker-compose.test.yml.
POSTGRES_IMAGE="postgres:16-alpine@sha256:16bc17c64a573ef34162af9298258d1aec548232985b33ed7b1eac33ba35c229"
POSTGRES_PASSWORD="rust-ci-test"

if [ -z "${GITHUB_TOKEN:-}" ]; then
  echo "rust-ci-test: GITHUB_TOKEN is required (see docs/DEV.md)" >&2
  exit 1
fi

name="v-note-rust-ci-pg-${CI_PIPELINE_NUMBER:-local}-$$"
started=$(date +%s)

# The traps below cannot run if the step is SIGKILLed (a Woodpecker cancel or
# timeout), which would leave this container running on the CI host. Reap any
# labelled sidecar more than an hour old first; a live run is never that old,
# so concurrent pipelines are safe.
docker ps --filter label=v-note.rust-ci-test=true \
  --format '{{.ID}} {{.Label "v-note.rust-ci-test.started"}}' |
  while read -r id since; do
    # No start stamp: not a sidecar this reaper can age, so leave it alone.
    case "$since" in '' | *[!0-9]*) continue ;; esac
    if [ $((started - since)) -gt 3600 ]; then
      docker rm -f "$id" >/dev/null 2>&1 || true
    fi
  done

cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

docker run -d --rm --name "$name" \
  --label v-note.rust-ci-test=true --label v-note.rust-ci-test.started="$started" \
  -e POSTGRES_USER=v_note -e POSTGRES_PASSWORD="$POSTGRES_PASSWORD" -e POSTGRES_DB=v_note \
  "$POSTGRES_IMAGE" >/dev/null

# Probe over TCP on purpose: the image's first-boot init runs a temporary server
# on the Unix socket only, so a socket probe can pass just before that restarts.
ready=""
for _ in $(seq 1 60); do
  if docker exec "$name" pg_isready -h 127.0.0.1 -U v_note -d v_note >/dev/null 2>&1; then
    ready=yes
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "rust-ci-test: postgres did not become ready within 60s" >&2
  docker logs "$name" >&2 || true
  exit 1
fi

ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$name")
if [ -z "$ip" ]; then
  echo "rust-ci-test: could not read the postgres container's bridge IP" >&2
  exit 1
fi

docker build -f Dockerfile.rust-ci --target test --output type=cacheonly \
  --secret id=github_token,env=GITHUB_TOKEN \
  --build-arg DATABASE_URL="postgres://v_note:${POSTGRES_PASSWORD}@${ip}:5432/v_note" \
  .
