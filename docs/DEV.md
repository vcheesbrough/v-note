# Local development — v-note

**Spec:** [`PLAN.md`](PLAN.md) · **Agent rules:** [`AGENTS.md`](../AGENTS.md)

---

## Prerequisites

| Tool | Purpose |
| --- | --- |
| **Rust** (stable) | Server, `crates/protocol`, Leptos frontend |
| **Docker** + Compose | Local stack, e2e reproduction |
| **Trunk** | `cargo install trunk` — Leptos SPA |
| **Node.js** 18+ | Playwright (`e2e/`) |
| **just** | Convenience targets (`justfile`) |
| **Android Studio** (Windows) | Emulator, USB device, SDK, `adb` — **recommended on WSL** |
| **NetBird / LAN** | Reach deployed dev env (optional) |

No dev container — Android local run needs Studio/emulator or a USB device; Docker is for **build-only** (`just build-android-docker`). All Android Docker paths pin **`scripts/android-build-box-image.ref`** (CI, `Dockerfile.android*`, `just`). App Links fingerprint: **`scripts/android-dev-debug-fingerprint.sh`** uses local `keytool` by default (`--docker` is the slow CI-parity path).

---

## Just targets

```bash
just run-server          # cargo run -p server  → http://localhost:8080
just run-spa             # trunk serve (frontend/)
just run-compose         # deploy/docker-compose.yml → https://localhost:8443
just build-android       # host Gradle → devDebug APK
just build-android-docker # Docker build (no local SDK)
just android-run         # reverse + install + launch (server must be up)
just contract-validation # cargo test -p protocol (fixture round-trip)
just e2e                 # Playwright via e2e/docker-compose.test.yml
```

---

## Server

```bash
cargo run -p server
curl http://localhost:8080/health
curl http://localhost:8080/api/meta
```

Environment:

| Variable | Default | Notes |
| --- | --- | --- |
| `PORT` | `8080` | HTTP listen (no TLS) |
| `APP_VERSION` | workspace `0.1.0` | exposed in `/api/meta` |
| `STATIC_DIR` | unset | when set, serves SPA + fallback `index.html` |
| `DATABASE_URL` | unset | sqlx migrations dir present; optional Postgres |
| `TLS_CERT` / `TLS_KEY` | unset | Docker image sets self-signed TLS on `:443` |
| `OIDC_ISSUER_URL` | **required** | OIDC issuer (mock-oidc locally — `deploy/compose.env`) |
| `OIDC_AUTHORIZE_URL` | optional | Browser-facing `/authorize` URL when it differs from discovery (local mock on `localhost:18080`) |
| `OIDC_CLIENT_ID` | **required** | SPA confidential client |
| `OIDC_CLIENT_SECRET` | **required** | SPA client secret (`test-secret` for local mock OIDC) |
| `OIDC_REDIRECT_URI` | **required** | e.g. `https://v-notes-dev.desync.link/auth/callback` |
| `REQUIRED_SCOPE` | **required** | `v-note:dev:access` or `v-note:prod:access` |
| `OIDC_END_SESSION_URL` | optional | RP-initiated logout redirect |
| `OIDC_ANDROID_CLIENT_ID` | optional | Android Authentik app client id (`v-note-android-{dev,prod}`) |
| `OIDC_ANDROID_ISSUER_URL` | optional | Android provider issuer (separate Authentik app) |
| `ASSETLINKS_JSON` | optional | Android App Links JSON at `/.well-known/assetlinks.json` |
| `METRICS_ADDR` | `0.0.0.0:9090` | internal Prometheus listener; set `disabled` to turn it off |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset | when set, exports OTLP traces to Alloy, e.g. `http://monitor-alloy:4317` |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` | only gRPC is supported |
| `OTEL_SERVICE_NAME` | `v-note` | trace service name |

**OIDC is mandatory:** the server refuses to start without `OIDC_ISSUER_URL` and related vars. Local dev and CI use **mock OIDC** (`deploy/docker-compose.local.yml`, `e2e/docker-compose.test.yml`) — not auth-disabled anonymous mode.

**E2e auth:** `e2e/docker-compose.test.yml` runs mock OIDC; Playwright `global-setup.ts` seeds the `auth` cookie. See `e2e/tests/auth.spec.ts`.

### Observability

Server logs are JSON on stdout/stderr. REST responses echo `X-Request-Id` and `X-Correlation-Id`; the SPA sends `X-Request-Id` on HTTP calls, and Android sends it on HTTP plus WSS handshakes.

Metrics are served as Prometheus text on the internal metrics listener (`METRICS_ADDR`, default `0.0.0.0:9090`). The main app router does not expose `/metrics` through Traefik. Local checks:

```bash
METRICS_ADDR=127.0.0.1:9090 cargo run -p server
curl http://127.0.0.1:9090/metrics
```

---

## SPA

```bash
trunk serve --config frontend/Trunk.toml frontend/index.html
```

Production build (also runs in Docker):

```bash
cd frontend && trunk build --release
```

---

## Android (chosen local workflow)

**One-time setup:** install [Android Studio](https://developer.android.com/studio) on **Windows**. In SDK Manager, install **API 35** + platform-tools. Enable USB debugging on Tab/Note 9, or create an emulator (e.g. Pixel Tablet).

Repo stays in **WSL**; `scripts/android-env.sh` finds Studio’s JDK/SDK under `/mnt/c/...` and writes `android/local.properties` if needed. Override with [`android/local.properties.example`](../android/local.properties.example).

### Flavors

| Flavor | `BASE_URL` | Use case |
| --- | --- | --- |
| **`dev`** | `https://v-notes-dev.desync.link` | Phone/tablet on LAN/mesh — no local server needed |
| **`devLocal`** | `http://127.0.0.1:8080` | Laptop emulator/USB with `adb reverse` |
| **`prod`** | `https://v-notes.desync.link` | Deployed prod stack only |

`dev` and `devLocal` share the same package ID (`link.desync.vnote.dev`) and Authentik OIDC config — installing one replaces the other.

### Daily loop (laptop, local server)

```bash
# Terminal 1
just run-server

# Terminal 2 — build devLocal, reverse, install, launch
just android-run
```

Manual steps:

```bash
just build-android          # builds devLocal APK (loopback)
just android-reverse
adb install -r android/app/build/outputs/apk/devLocal/debug/app-devLocal-debug.apk
```

### Phone/tablet on LAN/mesh (no local server)

Download the `dev` APK from the deployed dev stack after a CI push (the SPA links to it):

```
https://v-notes-dev.desync.link/dl/apk
```

The URL stays stable. Its download header supplies the CI release filename, for example
`v-note-0.13.1-dev-debug.apk`; use `curl -LOJ https://v-notes-dev.desync.link/dl/apk` to retain it.

Or build locally and sideload:

```bash
just build-android-docker   # CI-parity: builds dev APK (remote stack)
adb install -r android/app/build/outputs/apk/dev/debug/app-dev-debug.apk
```

Unit tests (host or Docker):

```bash
source scripts/android-env.sh && cd android && ./gradlew :app:testDevDebugUnitTest
```

Instrumented tests: `./gradlew :app:connectedDevDebugAndroidTest` with emulator running, or CI-parity `just android-instrumented-docker` (Woodpecker `android-instrumented` step).

### Emulator (WSL2 / Hyper-V)

On **Windows + WSL2**, do **not** install the **Android Emulator hypervisor driver (AEHD)**. It conflicts with Hyper-V/WSL2 and often fails with:

```text
[SC] StartService FAILED with error 4294967201
```

**Chosen path (keep WSL2 enabled):**

1. SDK Manager → skip or uninstall **Android Emulator hypervisor driver**.
2. Windows **Turn Windows features on or off** → enable **Windows Hypervisor Platform**, **Virtual Machine Platform**, and **Windows Subsystem for Linux** → reboot.
3. Create an AVD in Android Studio (e.g. Pixel Tablet, **x86_64** Google APIs system image).
4. Run the emulator from **Android Studio on Windows** (not from WSL).
5. Verify acceleration (PowerShell or cmd):

   ```text
   %LOCALAPPDATA%\Android\Sdk\emulator\emulator-check.exe accel
   ```

   Expect **WHPX** (or WHPX(10.0.22000.0)), not AEHD.

**Do not** run `bcdedit /set hypervisorlaunchtype off` — that disables Hyper-V and breaks WSL2.

WSL builds/install via `adb` (USB or emulator started on Windows); `just android-reverse` forwards port 8080 to the dev server in WSL.

---

## Compose (local)

```bash
export BAO_ADDR=https://secrets.desync.link
export BAO_TOKEN=<token with read on secret/v-note-stack/env>
./scripts/fetch-compose-env.sh   # writes deploy/.env from OpenBao + deploy/compose.env
just run-compose
```

Non-secret compose defaults are in **`deploy/compose.env`** (committed). Secrets (**`POSTGRES_PASSWORD`**, **`OIDC_CLIENT_SECRET`**) live in OpenBao **`secret/v-note-stack/env`**. Seed with **`scripts/patch-v-note-openbao-secrets.sh`** (operator).

Or: `./scripts/fetch-compose-env.sh` then `docker compose --env-file deploy/.env -f deploy/docker-compose.yml -f deploy/docker-compose.local.yml up --build`

- **API + SPA:** `https://localhost:8443` (self-signed — use `curl -k`)
- **Metrics:** `http://localhost:9090/metrics`
- **Postgres:** internal only (`postgres:5432`)

Mini deploy uses `deploy/docker-compose.yml` only (Traefik `proxy-backend`, `lan-vpn-only@docker`). Local dev adds `deploy/docker-compose.local.yml` (mock OIDC on `:18080`, published `:8443`, Traefik off). The server talks to **`mock-oidc:8080`** on the compose network; the browser sign-in redirect uses **`http://localhost:18080`** via optional **`OIDC_AUTHORIZE_URL`**.

---

## CI reproduction

```bash
docker build -f Dockerfile.web -t v-note:local .  # add --label flags from .woodpecker/build.yml build-web for OCI metadata
cargo test -p protocol -p server
TEST_IMAGE=v-note:local docker compose -f e2e/docker-compose.test.yml up \
  --build --force-recreate --abort-on-container-exit --exit-code-from playwright
```

Verify GitHub status after push:

```bash
SHA=$(git rev-parse HEAD)
gh api repos/vcheesbrough/v-note/commits/$SHA/status --jq '.state'
```

---

## Versioning

Two version numbers, deliberately:

- **Cargo `major.minor.patch`** in root `Cargo.toml` — source of `major.minor` only; the patch digit is a placeholder. `./scripts/sync-version.sh` propagates it to `version.txt` → Android `versionName`. `version.txt` is gitignored (regenerated at build).
- **CI release tag** (`compute-version` → `.release-tag`) — `major.minor` from cargo + **patch from git tag count**. This is the real deployed version (image tag, server `APP_VERSION`, `/api/meta`).

CI injects the release tag into both clients via **`V_NOTE_RELEASE`** (`--build-arg` → Dockerfile `ENV`) so the version watermark matches the deployed release:

- **SPA:** `option_env!("V_NOTE_RELEASE")` (frontend), falls back to `CARGO_PKG_VERSION`.
- **Android:** `System.getenv("V_NOTE_RELEASE")` (`build.gradle.kts`), falls back to `version.txt`.

Local builds (`just build-android`, `cargo`/`trunk` directly) don't set `V_NOTE_RELEASE`, so the watermark shows the cargo version (e.g. `0.4.0`) — fine for dev. A CI build of the same commit shows the real tag (e.g. `0.4.1`).

See [`PLAN.md`](PLAN.md) **Engineering workflows** → **Versioning** and **Client–server version alignment**.
