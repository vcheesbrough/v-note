#!/usr/bin/env bash
# Fail if mingc/android-build-box pin drifts from scripts/android-build-box-image.ref.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REF="$("$ROOT/scripts/android-build-box-image.sh")"

# A file either embeds the pin as a literal, in which case that literal must be
# the current one, or it resolves the ref at build time and cannot drift at all.
# Keep the two apart: a file that embeds a *stale* literal must not be rescued by
# also happening to mention the ref file in a comment, which is what let the
# digest in Dockerfile.android-instrumented go unchecked once it was split into
# name + digest and no longer matched the whole-ref grep.
# Embedding the pin means naming the image with a tag or digest attached — as
# opposed to mentioning it in prose (build.yml has such a comment) or resolving
# the .ref at build time.
embeds_pin() {
  grep -qE 'mingc/android-build-box[:@]' "$1"
}

check_file_uses_ref() {
  local file="$1"
  if embeds_pin "$file"; then
    # Whole ref on one line (Dockerfile.android), or name and digest on separate
    # lines (Dockerfile.android-instrumented, which feeds both its FROM and its
    # OCI base.* labels from them).
    if grep -qF "$REF" "$file"; then
      return 0
    fi
    if grep -qF "${REF%@*}" "$file" && grep -qF "${REF#*@}" "$file"; then
      return 0
    fi
    echo "ERROR: $file embeds an android-build-box pin that is not $REF" >&2
    return 1
  fi

  # Genuine consumption: reading the .ref file, or substituting the .sh helper.
  # A bare mention of the helper's name is not enough — .woodpecker/build.yml
  # invokes *this checker*, whose name ends in `android-build-box-image.sh`, and
  # matching that made build.yml's own pin check vacuous.
  if grep -qE 'android-build-box-image\.ref|\$\(.*android-build-box-image\.sh' "$file"; then
    return 0
  fi
  echo "ERROR: $file does not read the android-build-box pin from android-build-box-image.ref/.sh" >&2
  return 1
}

check_file_uses_ref "$ROOT/justfile"
check_file_uses_ref "$ROOT/.woodpecker/build.yml"
check_file_uses_ref "$ROOT/Dockerfile.android"
check_file_uses_ref "$ROOT/Dockerfile.android-instrumented"
check_file_uses_ref "$ROOT/scripts/android-dev-debug-fingerprint.sh"

echo "android-build-box image pin OK: $REF"
