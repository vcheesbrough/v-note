set shell := ["bash", "-cu"]

run-server:
    cargo run -p server

run-spa:
    trunk serve --config frontend/Trunk.toml frontend/index.html

run-compose:
    docker compose -f deploy/docker-compose.yml up --build

# Host JDK + SDK (Android Studio). See docs/DEV.md → Android.
build-android:
    ./scripts/sync-version.sh
    source scripts/android-env.sh
    cd android && bash ./gradlew :app:assembleDevDebug

# CI-parity build when JDK/SDK are not installed locally.
build-android-docker:
    ./scripts/sync-version.sh
    docker run --rm -e GRADLE_USER_HOME=/workspace/android/.gradle-user -v "{{justfile_directory()}}:/workspace" -w /workspace/android mingc/android-build-box:latest bash -lc './gradlew --project-cache-dir /workspace/android/.gradle-user/project-cache -Pandroid.sdk.dir=/opt/android-sdk :app:assembleDevDebug :app:testDevDebugUnitTest'

# Forward device/emulator port 8080 → host server (run once per adb device).
android-reverse:
    source scripts/android-env.sh
    adb reverse tcp:8080 tcp:8080

android-install: build-android android-reverse
    source scripts/android-env.sh
    adb install -r android/app/build/outputs/apk/dev/debug/app-dev-debug.apk

# Server must be running: `just run-server` in another terminal.
android-run: android-install
    source scripts/android-env.sh
    adb shell am start -n link.desync.vnote.dev/link.desync.vnote.MainActivity

e2e:
    TEST_IMAGE=v-note:local docker compose -f e2e/docker-compose.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright

contract-validation:
    cargo test -p protocol
