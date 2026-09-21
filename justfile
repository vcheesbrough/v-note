set shell := ["bash", "-cu"]

# Pin: scripts/android-build-box-image.ref (must match Dockerfile.android* build-arg).
android_build_box_image := trim(read('scripts/android-build-box-image.ref'))

run-server:
    cargo run -p server

run-spa:
    trunk serve --config frontend/Trunk.toml frontend/index.html

# Build the web image, then bring the local stack up on it. deploy/docker-compose.yml
# has no `build:` block — it defines production, and the web image's canonical build
# is CI — so the build happens here, from the same Dockerfile with the same args.
run-compose:
    ./scripts/fetch-compose-env.sh
    VERSION="$(git describe --tags --always --dirty)" && \
    docker build -f Dockerfile.web -t registry.desync.link/v-note:local \
      --secret id=github_token,env=GITHUB_TOKEN \
      --build-arg V_NOTE_RELEASE="$VERSION" \
      --build-arg OCI_IMAGE_VERSION="$VERSION" \
      --build-arg OCI_IMAGE_REVISION="$(git rev-parse HEAD)" \
      --build-arg OCI_IMAGE_CREATED="$(git log -1 --format=%cI)" \
      .
    APP_VERSION="$(git describe --tags --always --dirty)" \
    CLIENT_TELEMETRY_ALLOY_CONFIG="$(cat deploy/alloy/client-telemetry.alloy)" \
    docker compose --env-file deploy/.env -f deploy/docker-compose.yml -f deploy/docker-compose.local.yml up

# Host JDK + SDK (Android Studio). Builds devLocal (loopback). See docs/DEV.md → Android.
build-android:
    ./scripts/sync-version.sh
    source scripts/android-env.sh
    cd android && bash ./gradlew :app:assembleDevLocalDebug

# CI-parity build when JDK/SDK are not installed locally.
build-android-docker:
    ./scripts/sync-version.sh
    docker run --rm -e GRADLE_USER_HOME=/workspace/android/.gradle-user -v "{{justfile_directory()}}:/workspace" -w /workspace/android {{android_build_box_image}} bash -lc './gradlew --project-cache-dir /workspace/android/.gradle-user/project-cache -Pandroid.sdk.dir=/opt/android-sdk :app:assembleDevDebug :app:testDevDebugUnitTest'

# CI-parity Android static gates (ktlint, detekt, Android Lint), exactly as the
# Dockerfile.android builder stage runs them. Add `:app:detektBaseline` to refresh the
# detekt baseline deliberately — never to make a new finding go away.
android-lint-docker:
    ./scripts/sync-version.sh
    docker run --rm -e GRADLE_USER_HOME=/workspace/android/.gradle-user -v "{{justfile_directory()}}:/workspace" -w /workspace/android {{android_build_box_image}} bash -lc './gradlew --project-cache-dir /workspace/android/.gradle-user/project-cache -Pandroid.sdk.dir=/opt/android-sdk :app:ktlintCheck :app:detekt :app:lintDevDebug'

# CI-parity instrumented tests (emulator inside container; needs --privileged + /dev/kvm).
android-instrumented-docker api="36":
    ./scripts/sync-version.sh
    REF='{{android_build_box_image}}' && \
    docker build -f Dockerfile.android --target instrumented \
      --build-arg BUILD_BOX_IMAGE_NAME="${REF%@*}" \
      --build-arg BUILD_BOX_IMAGE_DIGEST="${REF#*@}" \
      --build-arg ANDROID_API_LEVEL={{api}} \
      --build-arg OCI_IMAGE_VERSION="$(git describe --tags --always --dirty)" \
      --build-arg OCI_IMAGE_REVISION="$(git rev-parse HEAD)" \
      --build-arg OCI_IMAGE_CREATED="$(git log -1 --format=%cI)" \
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
    export OCI_IMAGE_VERSION=local OCI_IMAGE_REVISION="$(git rev-parse HEAD)" OCI_IMAGE_CREATED="$(git log -1 --format=%cI)" && \
    TEST_IMAGE=v-note:local docker compose -f e2e/docker-compose.test.yml -f e2e/docker-compose.android-apk.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright

contract-validation:
    cargo test -p protocol

# CI-parity Rust gates, exactly as the `lint` / `rust-test` steps run them (needs GITHUB_TOKEN
# for the private sovereign-config dep). `just rust-ci lint` or `just rust-ci test`; `test`
# also starts the throwaway Postgres the `postgres-tests` feature needs.
rust-ci target="test":
    if [ "{{target}}" = "test" ]; then \
      ./scripts/rust-ci-test.sh; \
    else \
      docker build -f Dockerfile.rust-ci --target {{target}} --output type=cacheonly \
        --secret id=github_token,env=GITHUB_TOKEN .; \
    fi
