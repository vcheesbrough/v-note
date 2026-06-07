#!/usr/bin/env bash
# Fail if mingc/android-build-box pin drifts from scripts/android-build-box-image.ref.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REF="$("$ROOT/scripts/android-build-box-image.sh")"

check_file_uses_ref() {
  local file="$1"
  if grep -qF "$REF" "$file"; then
    return 0
  fi
  if grep -q 'android-build-box-image.ref' "$file"; then
    return 0
  fi
  if grep -q 'android-build-box-image.sh' "$file"; then
    return 0
  fi
  if grep -q 'ANDROID_BUILD_BOX_IMAGE' "$file"; then
    return 0
  fi
  echo "ERROR: $file does not use android-build-box pin ($REF)" >&2
  return 1
}

check_file_uses_ref "$ROOT/justfile"
check_file_uses_ref "$ROOT/.woodpecker/build.yml"
check_file_uses_ref "$ROOT/Dockerfile.android"
check_file_uses_ref "$ROOT/Dockerfile.android-instrumented"
check_file_uses_ref "$ROOT/scripts/android-dev-debug-fingerprint.sh"

echo "android-build-box image pin OK: $REF"
