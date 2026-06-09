#!/usr/bin/env bash
# Print SHA-256 (colon-separated) for the devDebug signing cert — seeds App Links.
#
# Default: the committed keystore android/app/debug.keystore via keytool (seconds).
# Every build (CI, docker, Studio) signs with this key, so its fingerprint is canonical.
#   --docker: CI image + Gradle signingReport (~minutes; parity check only).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

sha_from_keystore() {
  local store="$1"
  local alias
  for alias in androiddebugkey AndroidDebugKey; do
    if out="$(keytool -list -v -keystore "$store" -alias "$alias" \
      -storepass android -keypass android 2>/dev/null)"; then
      echo "$out" | awk -F': ' '/SHA-?256: / { print $2; exit }' | tr -d ' '
      return 0
    fi
  done
  return 1
}

find_local_debug_keystore() {
  local candidate
  # Committed keystore first — it is what every build actually signs with.
  if [[ -f "$ROOT/android/app/debug.keystore" ]]; then
    echo "$ROOT/android/app/debug.keystore"
    return 0
  fi
  for candidate in \
    "${ANDROID_SDK_HOME:-$HOME/.android}/debug.keystore" \
    "$HOME/.android/debug.keystore"; do
    if [[ -f "$candidate" ]]; then
      echo "$candidate"
      return 0
    fi
  done
  if [[ -d /mnt/c/Users ]]; then
    for win_home in /mnt/c/Users/*; do
      [[ "$(basename "$win_home")" == "Public" ]] && continue
      candidate="$win_home/.android/debug.keystore"
      if [[ -f "$candidate" ]]; then
        echo "$candidate"
        return 0
      fi
    done
  fi
  return 1
}

fingerprint_local() {
  local store
  if ! store="$(find_local_debug_keystore)"; then
    echo "ERROR: no debug keystore found (~/.android/debug.keystore)." >&2
    echo "  Build once: Android Studio, or \`just build-android\`, then re-run." >&2
    echo "  CI container keystore (slow): $0 --docker" >&2
    return 1
  fi
  echo "==> Reading debug keystore: $store" >&2
  sha_from_keystore "$store"
}

fingerprint_docker() {
  local image gradle_log sha
  image="$("$ROOT/scripts/android-build-box-image.sh")"
  echo "==> Docker + Gradle (cold: several minutes, little log until Gradle starts)…" >&2
  echo "==> Image: $image" >&2
  gradle_log="$(mktemp)"
  trap 'rm -f "$gradle_log"' RETURN
  docker run --rm -v "$ROOT:/workspace" -w /workspace/android "$image" \
    bash -lc 'echo "==> assembleDevDebug + signingReport inside container…" >&2; ./gradlew --no-daemon --console=plain -Pandroid.sdk.dir=/opt/android-sdk :app:assembleDevDebug :app:signingReport' \
    >"$gradle_log" 2>&1
  sha="$(awk '
      $0 ~ /^Variant: devDebug$/ { show=1; next }
      show && /^Variant:/ { exit }
      show && /SHA-256:/ {
        sub(/^.*SHA-256: /, "")
        gsub(/ /, "")
        print
        exit
      }
    ' "$gradle_log")"
  if [[ -z "$sha" ]]; then
    echo "ERROR: no devDebug SHA-256 in Gradle signingReport (see $gradle_log)" >&2
    return 1
  fi
  echo "$sha"
}

case "${1:-}" in
  --docker) fingerprint_docker ;;
  -h|--help)
    echo "Usage: $0 [--docker]"
    echo "  default  local debug.keystore via keytool (fast)"
    echo "  --docker CI android-build-box + Gradle (slow)"
    exit 0
    ;;
  "") fingerprint_local ;;
  *)
    echo "ERROR: unknown option: $1 (try --help)" >&2
    exit 1
    ;;
esac
