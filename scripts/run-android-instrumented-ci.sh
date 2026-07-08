#!/usr/bin/env bash
# CI: boot the pre-baked Android emulator and run devDebug instrumented tests.
set -euo pipefail

export ANDROID_HOME="${ANDROID_HOME:-/opt/android-sdk}"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export PATH="$ANDROID_HOME/emulator:$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$PATH"
export ADB_INSTALL_TIMEOUT=120

AVD_NAME="${AVD_NAME:-vnote-ci}"
SYSTEM_IMAGE="${SYSTEM_IMAGE:-system-images;android-35;google_apis;x86_64}"
BOOT_TIMEOUT_SEC="${BOOT_TIMEOUT_SEC:-900}"
APP_APK="${APP_APK:-/workspace/android/app/build/outputs/apk/dev/debug/app-dev-debug.apk}"
TEST_APK="${TEST_APK:-/workspace/android/app/build/outputs/apk/androidTest/dev/debug/app-dev-debug-androidTest.apk}"
TEST_RUNNER="${TEST_RUNNER:-link.desync.vnote.dev.test/androidx.test.runner.AndroidJUnitRunner}"

ensure_avd() {
  if avdmanager list avd 2>/dev/null | grep -q "Name: ${AVD_NAME}"; then
    echo "Using pre-baked AVD: ${AVD_NAME}"
    return 0
  fi

  echo "AVD ${AVD_NAME} missing — installing ${SYSTEM_IMAGE}" >&2
  local attempt
  for attempt in 1 2 3; do
    rm -rf "${ANDROID_HOME}/.temp" 2>/dev/null || true
    if yes | sdkmanager --licenses >/dev/null 2>&1 \
      && sdkmanager "platform-tools" "emulator" "$SYSTEM_IMAGE"; then
      echo no | avdmanager create avd -n "$AVD_NAME" -k "$SYSTEM_IMAGE" -d pixel_6 --force
      return 0
    fi
    echo "sdkmanager attempt ${attempt} failed; retrying..." >&2
    sleep 10
  done
  echo "Failed to provision emulator system image after 3 attempts" >&2
  return 1
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

if [ ! -c /dev/kvm ]; then
  echo "WARNING: /dev/kvm not available — x86_64 emulator will be slow without hardware acceleration" >&2
fi

cd /workspace
./scripts/sync-version.sh
ensure_avd

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
if [ -f "$APP_APK" ] && [ -f "$TEST_APK" ]; then
  echo "Installing prebuilt app APK: $APP_APK"
  adb install -r -t "$APP_APK"
  echo "Installing prebuilt test APK: $TEST_APK"
  adb install -r -t "$TEST_APK"
  echo "Running instrumentation: $TEST_RUNNER"
  adb shell am instrument -w "$TEST_RUNNER"
else
  echo "Prebuilt APKs not found; falling back to Gradle connected test" >&2
  ./gradlew --no-daemon --console=plain -Pandroid.sdk.dir="$ANDROID_HOME" \
    :app:connectedDevDebugAndroidTest
fi
