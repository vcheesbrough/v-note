set shell := ["bash", "-cu"]

# Pin: scripts/android-build-box-image.ref (must match Dockerfile.android* build-arg).
android_build_box_image := trim(read('scripts/android-build-box-image.ref'))

run-server:
    cargo run -p server

run-spa:
    trunk serve --config frontend/Trunk.toml frontend/index.html

run-compose:
    ./scripts/fetch-compose-env.sh
    docker compose --env-file deploy/.env -f deploy/docker-compose.yml -f deploy/docker-compose.local.yml up --build

# Host JDK + SDK (Android Studio). See docs/DEV.md → Android.
build-android:
    ./scripts/sync-version.sh
    source scripts/android-env.sh
    cd android && bash ./gradlew :app:assembleDevDebug

# CI-parity build when JDK/SDK are not installed locally.
build-android-docker:
    ./scripts/sync-version.sh
    docker run --rm -e GRADLE_USER_HOME=/workspace/android/.gradle-user -v "{{justfile_directory()}}:/workspace" -w /workspace/android {{android_build_box_image}} bash -lc './gradlew --project-cache-dir /workspace/android/.gradle-user/project-cache -Pandroid.sdk.dir=/opt/android-sdk :app:assembleDevDebug :app:testDevDebugUnitTest'

# CI-parity instrumented tests (emulator inside container; needs --privileged + /dev/kvm).
android-instrumented-docker:
    ./scripts/sync-version.sh
    docker build -f Dockerfile.android-instrumented --build-arg ANDROID_BUILD_BOX_IMAGE={{android_build_box_image}} -t v-note-android-instrumented:local .
    if [ -c /dev/kvm ]; then
      docker run --rm --privileged --device=/dev/kvm v-note-android-instrumented:local
    else
      docker run --rm --privileged v-note-android-instrumented:local
    fi

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
