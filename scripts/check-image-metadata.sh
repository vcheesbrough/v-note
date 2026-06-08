#!/usr/bin/env bash
# Assert required OCI image labels on v-note-built images (web, android, instrumented, e2e playwright).
set -euo pipefail

IMAGE="${1:?usage: check-image-metadata.sh <image-ref>}"
EXPECTED_VERSION="${2:-}"
EXPECTED_REVISION="${3:-}"

label() {
  docker image inspect --format "{{ index .Config.Labels \"$1\" }}" "$IMAGE" 2>/dev/null || true
}

require_label() {
  local key="$1"
  local value
  value="$(label "$key")"
  if [[ -z "$value" ]]; then
    echo "ERROR: missing label $key on $IMAGE" >&2
    exit 1
  fi
  echo "  $key=$value"
}

echo "==> OCI labels on $IMAGE"
require_label org.opencontainers.image.title
require_label org.opencontainers.image.description
require_label org.opencontainers.image.licenses
require_label org.opencontainers.image.source
require_label org.opencontainers.image.url
require_label org.opencontainers.image.authors
require_label org.opencontainers.image.documentation
require_label org.opencontainers.image.base.name
require_label org.opencontainers.image.base.digest
VERSION="$(label org.opencontainers.image.version)"
REVISION="$(label org.opencontainers.image.revision)"
CREATED="$(label org.opencontainers.image.created)"

[[ -n "$VERSION" ]] || { echo "ERROR: missing org.opencontainers.image.version" >&2; exit 1; }
[[ -n "$REVISION" ]] || { echo "ERROR: missing org.opencontainers.image.revision" >&2; exit 1; }
[[ -n "$CREATED" ]] || { echo "ERROR: missing org.opencontainers.image.created" >&2; exit 1; }

echo "  org.opencontainers.image.version=$VERSION"
echo "  org.opencontainers.image.revision=$REVISION"
echo "  org.opencontainers.image.created=$CREATED"

if [[ -n "$EXPECTED_VERSION" && "$VERSION" != "$EXPECTED_VERSION" ]]; then
  echo "ERROR: version label '$VERSION' != expected '$EXPECTED_VERSION'" >&2
  exit 1
fi

if [[ -n "$EXPECTED_REVISION" && "$REVISION" != "$EXPECTED_REVISION" ]]; then
  echo "ERROR: revision label '$REVISION' != expected '$EXPECTED_REVISION'" >&2
  exit 1
fi

echo "==> OK"
