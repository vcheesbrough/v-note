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

No dev container — Android local run needs Studio/emulator or a USB device; Docker is for **build-only** (`just build-android-docker`).

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
| `OIDC_ISSUER_URL` | unset | When unset, auth is disabled (`/api/me` returns anonymous) |
| `OIDC_AUTHORIZE_URL` | optional | Browser-facing `/authorize` URL when it differs from discovery (local mock on `localhost:18080`) |
| `OIDC_CLIENT_ID` | required with issuer | SPA confidential client |
| `OIDC_CLIENT_SECRET` | required with issuer | SPA client secret |
| `OIDC_REDIRECT_URI` | required with issuer | e.g. `https://v-notes-dev.desync.link/auth/callback` |
| `REQUIRED_SCOPE` | required with issuer | `v-note:dev:access` or `v-note:prod:access` |
| `OIDC_END_SESSION_URL` | optional | RP-initiated logout redirect |
| `OIDC_ANDROID_CLIENT_ID` | optional | Android Authentik app client id (`v-note-android-{dev,prod}`) |
| `OIDC_ANDROID_ISSUER_URL` | optional | Android provider issuer (separate Authentik app) |
| `ASSETLINKS_JSON` | optional | Android App Links JSON at `/.well-known/assetlinks.json` |

**Local auth-disabled mode:** omit `OIDC_ISSUER_URL` — same as pre-#146 bootstrap.

**E2e auth:** `e2e/docker-compose.test.yml` runs mock OIDC; Playwright `global-setup.ts` seeds the `auth` cookie. See `e2e/tests/auth.spec.ts`.

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

**Dev flavor** `BASE_URL` is `http://127.0.0.1:8080`. Before each test session, forward the port (emulator **or** USB — same command):

```bash
adb reverse tcp:8080 tcp:8080
```

### Daily loop

```bash
# Terminal 1
just run-server

# Terminal 2 — build, install, launch dev APK
just android-run
```

Manual steps:

```bash
just build-android          # or just build-android-docker (no local SDK)
just android-reverse
adb install -r android/app/build/outputs/apk/dev/debug/app-dev-debug.apk
```

**Prod flavor** `BASE_URL`: `https://v-notes.desync.link` (deployed stack only).

Unit tests (host or Docker):

```bash
source scripts/android-env.sh && cd android && ./gradlew :app:testDevDebugUnitTest
```

Instrumented tests: `./gradlew :app:connectedDevDebugAndroidTest` with emulator running (`PlaceholderInstrumentedTest` scaffold).

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
- **Postgres:** internal only (`postgres:5432`)

Mini deploy uses `deploy/docker-compose.yml` only (Traefik `proxy-backend`, `lan-vpn-only@docker`). Local dev adds `deploy/docker-compose.local.yml` (mock OIDC on `:18080`, published `:8443`, Traefik off). The server talks to **`mock-oidc:8080`** on the compose network; the browser sign-in redirect uses **`http://localhost:18080`** via optional **`OIDC_AUTHORIZE_URL`**.

---

## CI reproduction

```bash
docker build -t v-note:local .
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

Workspace version in root `Cargo.toml` (`0.1.0` for iteration 1). `./scripts/sync-version.sh` propagates to `version.txt` → Android `versionName`.

See [`PLAN.md`](PLAN.md) **Engineering workflows** → **Versioning** and **Client–server version alignment**.
