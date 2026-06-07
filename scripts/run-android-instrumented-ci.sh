#!/usr/bin/env bash
# CI: boot an Android emulator and run devDebug instrumented tests.
set -euo pipefail

export ANDROID_HOME="${ANDROID_HOME:-/opt/android-sdk}"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export PATH="$ANDROID_HOME/emulator:$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$PATH"
export ADB_INSTALL_TIMEOUT=120

AVD_NAME="vnote-ci"
BOOT_TIMEOUT_SEC="${BOOT_TIMEOUT_SEC:-900}"

pick_system_image() {
  if [ -c /dev/kvm ]; then
    echo "system-images;android-35;google_apis;x86_64"
  else
    echo "system-images;android-35;google_apis;arm64-v8a"
  fi
}

wait_for_emulator() {
  adb wait-for-device
  local elapsed=0
  until adb shell getprop sys.boot_completed 2>/dev/null | grep -q 1; do
    sleep 5
    elapsed=$((elapsed + 5))
    if [ "$elapsed" -ge "$BOOT_TIMEOUT_SEC" ]; then
      echo "Emulator boot timed out after ${BOOT_TIMEOUT_SEC}s" >&2
      adb devices -l >&2 || true
      return 1
    fi
  done
  adb shell input keyevent 82 >/dev/null 2>&1 || true
}

cleanup() {
  adb emu kill >/dev/null 2>&1 || true
  pkill -f "emulator.*${AVD_NAME}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cd /workspace
./scripts/sync-version.sh

SYSTEM_IMAGE="$(pick_system_image)"
echo "Using system image: ${SYSTEM_IMAGE}"

yes | sdkmanager --licenses >/dev/null 2>&1 || true
sdkmanager "platform-tools" "emulator" "$SYSTEM_IMAGE"

if ! avdmanager list avd 2>/dev/null | grep -q "Name: ${AVD_NAME}"; then
  echo no | avdmanager create avd -n "$AVD_NAME" -k "$SYSTEM_IMAGE" -d pixel_6 --force
fi

EMULATOR_ARGS=(
  -avd "$AVD_NAME"
  -no-window
  -no-audio
  -no-boot-anim
  -gpu swiftshader_indirect
  -no-snapshot-save
  -no-snapshot-load
)

if [ -c /dev/kvm ]; then
  EMULATOR_ARGS+=(-accel on)
else
  EMULATOR_ARGS+=(-accel off -no-accel)
fi

"$ANDROID_HOME/emulator/emulator" "${EMULATOR_ARGS[@]}" &
wait_for_emulator

cd android
chmod +x ./gradlew
./gradlew --no-daemon --console=plain -Pandroid.sdk.dir="$ANDROID_HOME" \
  :app:connectedDevDebugAndroidTest
