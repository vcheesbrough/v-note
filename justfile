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

# Host JDK + SDK (Android Studio). Builds devLocal (loopback). See docs/DEV.md → Android.
build-android:
    ./scripts/sync-version.sh
    source scripts/android-env.sh
    cd android && bash ./gradlew :app:assembleDevLocalDebug

# CI-parity build when JDK/SDK are not installed locally.
build-android-docker:
    ./scripts/sync-version.sh
    docker run --rm -e GRADLE_USER_HOME=/workspace/android/.gradle-user -v "{{justfile_directory()}}:/workspace" -w /workspace/android {{android_build_box_image}} bash -lc './gradlew --project-cache-dir /workspace/android/.gradle-user/project-cache -Pandroid.sdk.dir=/opt/android-sdk :app:assembleDevDebug :app:testDevDebugUnitTest'

# CI-parity instrumented tests (emulator inside container; needs --privileged + /dev/kvm).
android-instrumented-docker api="36":
    ./scripts/sync-version.sh
    CREATED="$(date -u +%Y-%m-%dT%H:%M:%SZ)" SHA="$(git rev-parse HEAD)" && \
    docker build -f Dockerfile.android-instrumented \
      --build-arg ANDROID_BUILD_BOX_IMAGE={{android_build_box_image}} \
      --build-arg ANDROID_API_LEVEL={{api}} \
      --label org.opencontainers.image.title=v-note-android-instrumented \
      --label "org.opencontainers.image.description=v-note Android API {{api}} CI image (devDebug instrumented tests + emulator)" \
      --label org.opencontainers.image.licenses=PolyForm-Noncommercial-1.0.0 \
      --label org.opencontainers.image.url=https://github.com/vcheesbrough/v-note \
      --label org.opencontainers.image.authors="Vincent Cheesbrough" \
      --label org.opencontainers.image.vendor="Vincent Cheesbrough" \
      --label org.opencontainers.image.documentation=https://github.com/vcheesbrough/v-note/blob/master/docs/DEPLOY.md \
      --label org.opencontainers.image.base.name=mingc/android-build-box:master \
      --label org.opencontainers.image.base.digest=sha256:6644d9869eeecf26bc80894d00540483139f52f4aa8668c9f4ee873c82dd054c \
      --label org.opencontainers.image.version=local \
      --label org.opencontainers.image.revision="$SHA" \
      --label org.opencontainers.image.source=https://github.com/vcheesbrough/v-note \
      --label org.opencontainers.image.created="$CREATED" \
      -t v-note-android-instrumented:local-api{{api}} .
    if [ -c /dev/kvm ]; then
      docker run --rm --privileged --device=/dev/kvm v-note-android-instrumented:local-api{{api}}
    else
      docker run --rm --privileged v-note-android-instrumented:local-api{{api}}
    fi

# Forward device/emulator port 8080 → host server (run once per adb device).
android-reverse:
    source scripts/android-env.sh
    adb reverse tcp:8080 tcp:8080

android-install: build-android android-reverse
    source scripts/android-env.sh
    adb install -r android/app/build/outputs/apk/devLocal/debug/app-devLocal-debug.apk

# Server must be running: `just run-server` in another terminal.
android-run: android-install
    source scripts/android-env.sh
    adb shell am start -n link.desync.vnote.dev/link.desync.vnote.MainActivity

e2e:
    export OCI_IMAGE_VERSION=local OCI_IMAGE_REVISION="$(git rev-parse HEAD)" OCI_IMAGE_CREATED="$(date -u +%Y-%m-%dT%H:%M:%SZ)" && \
    TEST_IMAGE=v-note:local docker compose -f e2e/docker-compose.test.yml -f e2e/docker-compose.android-apk.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright

contract-validation:
    cargo test -p protocol
