#!/usr/bin/env bash
# Print the pinned mingc/android-build-box image (single source: android-build-box-image.ref).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tr -d '[:space:]' < "$ROOT/scripts/android-build-box-image.ref"
