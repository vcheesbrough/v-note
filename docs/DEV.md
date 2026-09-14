# Local development — v-note

**Spec:** [`PLAN.md`](PLAN.md) · **Agent rules:** [`AGENTS.md`](../AGENTS.md)

---

## Prerequisites

| Tool | Purpose |
| --- | --- |
| **Rust** | Server, `crates/protocol`, Leptos frontend — version pinned by [`rust-toolchain.toml`](../rust-toolchain.toml); rustup honours it automatically |
| **Docker** + Compose | Local stack, e2e reproduction |
| **Trunk** | `cargo install trunk --version 0.21.14 --locked` — Leptos SPA. `--locked` is required: an unlocked resolution of 0.21.14 no longer compiles |
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

Only three env vars are read directly; everything else goes through the config
system below.

| Variable | Default | Notes |
| --- | --- | --- |
| `SOVEREIGN_CONFIG_ACCESS_URL_FILE` / `SOVEREIGN_CONFIG_ACCESS_URL` | unset | **bootstrap** — path to (or inline) the sovereign-config access-URL secret; when set, runtime config is read from sovereign-config |
| `RUST_LOG` | `server=info,tower_http=info,axum=info` | standard tracing `EnvFilter` level |
| `V_NOTE_RELEASE` | crate version | **compile-time** — release tag baked into the binary (and the SPA) at build; shown in `/api/meta` and OTEL `service.version` |

### Runtime configuration (`VNOTE__*`)

Since iteration 19 the server's runtime config lives in **sovereign-config**
(`/v-note/{dev,prod}/server`), loaded as five independent groups —
`database`, `oidc`, `observability`, `android`, `server`. **The server refuses to
start (non-zero exit, redacted error) if any value is missing or invalid.**

Config is layered: in-memory defaults → sovereign-config (only when an access URL
is set) → `VNOTE__*` environment overrides. Local dev and e2e have no
sovereign-config server, so they supply every group through the env layer.

The `server` group is image-internal (identical across deployments), so it comes
from defaults or `VNOTE__SERVER__*` overrides set by the image. **Do not add a
`server/*` leaf to sovereign-config** — that is a convention, not an enforced
boundary: the sovereign layer is merged wholesale, so such a leaf would be picked
up (the image's env overrides out-rank it for `tls-cert`/`tls-key`/`static-dir`,
but `http-port` has no override).

`VNOTE__<GROUP>__<LEAF>` maps to `<group>.<leaf>`; `__` separates path segments.
Canonical leaf names are **kebab-case**, matching sovereign-config path segments
(which forbid `_`), so one key addresses the same value in every layer.

**Both spellings work.** `-` is not legal in a shell variable name — a plain
`VNOTE__OBSERVABILITY__METRICS-ADDR=… cargo run` is parsed as a *command*, not an
assignment — so the snake_case form is accepted too and folded onto the same key:

```bash
VNOTE__OBSERVABILITY__METRICS_ADDR=127.0.0.1:9090 cargo run -p server   # shell-friendly
VNOTE__OBSERVABILITY__METRICS-ADDR: "127.0.0.1:9090"                    # compose / sovereign-config
```

Use snake_case in a shell, kebab-case in compose and sovereign-config. Errors always
name the canonical kebab path. Blank optional values mean "absent".

| Variable | Default | Notes |
| --- | --- | --- |
| `VNOTE__DATABASE__HOST` | **required** | Postgres host |
| `VNOTE__DATABASE__PORT` | `5432` | coerced to `u16` |
| `VNOTE__DATABASE__NAME` / `__USER` | **required** | database + role |
| `VNOTE__DATABASE__PASSWORD` | **required** | secret leaf in sovereign-config |
| `VNOTE__OIDC__ISSUER-URL` | **required** | OIDC issuer (mock-oidc locally — `deploy/compose.env`) |
| `VNOTE__OIDC__AUTHORIZE-URL` | optional | browser-facing `/authorize` when it differs from discovery |
| `VNOTE__OIDC__CLIENT-ID` | **required** | SPA confidential client |
| `VNOTE__OIDC__CLIENT-SECRET` | **required** | secret leaf (`test-secret` for local mock OIDC) |
| `VNOTE__OIDC__REDIRECT-URI` | **required** | e.g. `https://v-notes-dev.desync.link/auth/callback` |
| `VNOTE__OIDC__REQUIRED-SCOPE` | **required** | `v-note:dev:access` or `v-note:prod:access` |
| `VNOTE__OIDC__END-SESSION-URL` | optional | RP-initiated logout redirect |
| `VNOTE__OIDC__ANDROID__CLIENT-ID` | optional | Android Authentik app client id |
| `VNOTE__OIDC__ANDROID__ISSUER-URL` | optional | Android provider issuer (separate Authentik app) |
| `VNOTE__OBSERVABILITY__ENVIRONMENT` | **required** | OTEL `deployment.environment` (`dev` \| `production`) |
| `VNOTE__OBSERVABILITY__OTLP-ENDPOINT` | unset | when set, exports OTLP traces to Alloy, e.g. `http://monitor-alloy:4317` |
| `VNOTE__OBSERVABILITY__OTLP-PROTOCOL` | `grpc` | only gRPC is supported; anything else fails startup |
| `VNOTE__OBSERVABILITY__OTLP-TIMEOUT-MS` | `2000` | coerced to `u64` |
| `VNOTE__OBSERVABILITY__SERVICE-NAME` | `v-note` | trace service name |
| `VNOTE__OBSERVABILITY__METRICS-ADDR` | `0.0.0.0:9090` | internal Prometheus listener; `disabled`/blank turns it off |
| `VNOTE__ANDROID__ASSETLINKS-JSON` | optional | Android App Links JSON at `/.well-known/assetlinks.json`; must parse as JSON |
| `VNOTE__SERVER__HTTP-PORT` | `8080` | plain-HTTP listen port, used only when TLS is unset |
| `VNOTE__SERVER__TLS-CERT` / `__TLS-KEY` | unset | PEM paths; when both set, binds TLS on `:443` (both-or-neither). The image sets these |
| `VNOTE__SERVER__STATIC-DIR` | unset | when set, serves the SPA + `index.html` fallback. The image sets `/app/dist` |

**OIDC is mandatory:** the server refuses to start without `VNOTE__OIDC__ISSUER-URL` and related leaves. Local dev and CI use **mock OIDC** (`deploy/docker-compose.local.yml`, `e2e/docker-compose.test.yml`) — not auth-disabled anonymous mode.

**E2e auth:** `e2e/docker-compose.test.yml` runs mock OIDC; Playwright `global-setup.ts` seeds the `auth` cookie. See `e2e/tests/auth.spec.ts`.

### Observability

Server logs are JSON on stdout/stderr. REST responses echo `X-Request-Id` and `X-Correlation-Id`; the SPA sends `X-Request-Id` on HTTP calls, and Android sends it on HTTP plus WSS handshakes.

Metrics are served as Prometheus text on the internal metrics listener
(`VNOTE__OBSERVABILITY__METRICS-ADDR`, default `0.0.0.0:9090`). The main app router
does not expose `/metrics` through Traefik. Local checks:

```bash
VNOTE__OBSERVABILITY__METRICS_ADDR=127.0.0.1:9090 cargo run -p server
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

The canonical URL stays stable. SPA links append the current release as a query value so Android
Chrome does not reuse an earlier release's download entry. The response header supplies the CI
release filename, for example `v-note-0.13.1-dev-debug.apk`; use
`curl -LOJ https://v-notes-dev.desync.link/dl/apk` to retain it.

Or build locally and sideload:

```bash
just build-android-docker   # CI-parity: builds dev APK (remote stack)
adb install -r android/app/build/outputs/apk/dev/debug/app-dev-debug.apk
```

Unit tests (host or Docker):

```bash
source scripts/android-env.sh && cd android && ./gradlew :app:testDevDebugUnitTest
```

Instrumented tests: `./gradlew :app:connectedDevDebugAndroidTest` with an emulator running. CI gates both supported device generations: Android 10/API 29 (Galaxy Note9) and Android 16/API 36 (Galaxy Tab S8 Ultra). Reproduce either lane with `just android-instrumented-docker 29` or `just android-instrumented-docker 36`; API 36 is the recipe default.

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
export GITHUB_TOKEN=<token with read on vcheesbrough/sovereign-config>  # for the image build
./scripts/fetch-compose-env.sh   # writes deploy/.env from OpenBao + deploy/compose.env
just run-compose
```

`just run-compose` builds `registry.desync.link/v-note:local` from `Dockerfile.web`
and then brings the stack up on it — one command, but two steps, because
`deploy/docker-compose.yml` defines production and deliberately has no `build:`
block (see [`DEPLOY.md`](DEPLOY.md) → OCI image metadata). The cargo layers fetch
the **private** `sovereign-config` git dep, so `GITHUB_TOKEN` must be set; it is
passed as a BuildKit secret (`--secret id=github_token,env=GITHUB_TOKEN`), the same
credential path CI uses, and is never mounted into the running container.

The build also stamps `OCI_IMAGE_VERSION` / `OCI_IMAGE_REVISION` /
`OCI_IMAGE_CREATED` and `V_NOTE_RELEASE` from git, so a local image's version
label, `/api/meta` and `v_note_build_info{version=…}` all agree.

Non-secret compose defaults are in **`deploy/compose.env`** (committed). Secrets (**`POSTGRES_PASSWORD`**, **`OIDC_CLIENT_SECRET`**) live in OpenBao **`secret/v-note-stack/env`**. Seed with **`scripts/patch-v-note-openbao-secrets.sh`** (operator).

The local overlay maps those into the `VNOTE__*` layer and blanks
`SOVEREIGN_CONFIG_ACCESS_URL_FILE`, so local dev never talks to sovereign-config.

Or run the two steps by hand: the `docker build` from `just run-compose`, then
`docker compose --env-file deploy/.env -f deploy/docker-compose.yml -f deploy/docker-compose.local.yml up`

- **API + SPA:** `https://localhost:8443` (self-signed — use `curl -k`)
- **Metrics:** `http://localhost:9090/metrics`
- **Postgres:** internal only (`postgres:5432`)
- **Health:** the container probes `/health` itself — `docker ps` shows
  `(healthy)`, and when it is not,
  `docker inspect -f '{{json .State.Health}}' v-note-local` shows the last five
  probe attempts with curl's own error text

Mini deploy uses `deploy/docker-compose.yml` only (Traefik `proxy-backend`, `lan-vpn-only@docker`). Local dev adds `deploy/docker-compose.local.yml` (mock OIDC on `:18080`, published `:8443`, Traefik off). The server talks to **`mock-oidc:8080`** on the compose network; the browser sign-in redirect uses **`http://localhost:18080`** via optional **`OIDC_AUTHORIZE_URL`**.

---

## CI reproduction

Building the web image needs a GitHub token: `sovereign-config-provider` is a git
dependency on a **private** repo, so the cargo layers fetch it with the same
BuildKit secret CI uses. A token with read access to `vcheesbrough/sovereign-config`
is enough. (`cargo run -p server` does not need this — it uses your host git
credentials directly.)

```bash
export GITHUB_TOKEN=<token with read on vcheesbrough/sovereign-config>

just rust-ci lint                                 # CI `lint`: clippy -D warnings, then fmt --check
just rust-ci test                                 # CI `rust-test`: cargo test -p protocol -p frontend -p server
docker build -f Dockerfile.web -t v-note:local --secret id=github_token,env=GITHUB_TOKEN .  # add --build-arg OCI_IMAGE_* for a labelled image (see DEPLOY.md)
./scripts/test-container-health.sh v-note:local   # HEALTHCHECK config + a real unhealthy transition
./scripts/test-deploy-v-note.sh                   # deploy parameter guards + health gate (no docker socket needed)
./scripts/check-android-build-box-image.sh        # CI `android-build-box-pin`
TEST_IMAGE=v-note:local docker compose -f e2e/docker-compose.test.yml up \
  --build --force-recreate --abort-on-container-exit --exit-code-from playwright
```

**Rust CI runs inside BuildKit.** `lint` and `rust-test` are `docker build`s of
`Dockerfile.rust-ci` (`--output type=cacheonly`, so no image is left behind). They
share one crate cache with `Dockerfile.web` — the whole `CARGO_HOME`, cache id
`v-note-cargo-home` — and each keep a compiled target dir (`v-note-cargo-target-lint`,
`v-note-cargo-target-test`, next to the release `v-note-cargo-target`), so a warm run
recompiles only the workspace crates. The shared cache is the `CARGO_HOME` *root* on
purpose: cargo's download lock lives there, so the three concurrent builds take turns
on it instead of colliding on the same git checkout. They are BuildKit cache mounts,
not volumes:

```bash
docker buildx du --filter type=exec.cachemount     # size of every cache mount
docker buildx prune --filter type=exec.cachemount  # drop them; the next build is cold
```

Because they are cache, BuildKit's garbage collector may evict them under disk
pressure — a run after an eviction is slower, never wrong.

**Push CI is four workflows** — `checks`, `web`, `android` (parallel) and `deploy`
(after all three). A commit is green only when every one of them is; the combined
GitHub status below reflects all of them.

The e2e stack waits on the app's healthcheck (`service_healthy`), so a container
that never becomes healthy fails the run with `dependency failed to start`
instead of surfacing as a confusing Playwright timeout.

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
