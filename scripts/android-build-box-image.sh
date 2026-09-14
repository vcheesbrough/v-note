#!/bin/sh
# Print the pinned mingc/android-build-box image (single source: android-build-box-image.ref).
# POSIX sh: the pin check calls this from the `checks` workflow on the docker CLI
# image, which ships no bash.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
tr -d '[:space:]' < "$ROOT/scripts/android-build-box-image.ref"
