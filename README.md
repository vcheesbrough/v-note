# v-note



Self-hosted ink note capture and sync: native Android stylus authoring, owner-only pages, realtime replay on a read-only web SPA, and handwriting search over private LAN or mesh.

## Status

**Iteration 2 (#146) merged** — `master` at **`0.2.0`**. MVP authentication: Authentik OIDC for **SPA** (cookie session), **Android** (PKCE + Keystore), and **API** (`GET /api/me`). Next: **#147** owner page library. **Task queue:** [v-notes Kanban board](https://bored.desync.link/boards/v-notes).

## Quick start

Prerequisites: **Rust** (pinned by `rust-toolchain.toml`), **Docker**, **Trunk** (`cargo install trunk --version 0.21.14 --locked`), **Node.js** (e2e), **JDK 17** + **Android SDK** (Android builds). See [`docs/DEV.md`](docs/DEV.md).

```bash
# Server (HTTP on :8080)
just run-server
curl http://localhost:8080/health
curl http://localhost:8080/api/meta
# Runtime config comes from sovereign-config, overridable via VNOTE__* env vars.
# A bare run needs the database and OIDC groups set — see docs/DEV.md, or use
# `just run-compose` for a ready-made local stack with mock OIDC.

# SPA (Trunk dev server — proxies API to server in dev)
just run-spa

# Full stack (Postgres + TLS server + SPA static on :8443)
just run-compose
curl -k https://localhost:8443/health
curl -k https://localhost:8443/api/meta

# Android — Android Studio on Windows; see docs/DEV.md
just run-server    # terminal 1
just android-run   # terminal 2: adb reverse + install + launch dev APK

# Contract fixtures
just contract-validation

# E2e (build web image first; see .woodpecker/web.yml build-web for the build args)
just e2e
```

## Repository layout

| Path | Purpose |
| --- | --- |
| `crates/server/` | Axum API (`/health`, `/api/meta`, `/api/me`, `/auth/*`) + static SPA |
| `crates/protocol/` | Shared serde types + fixture tests |
| `frontend/` | Leptos/Trunk WASM SPA placeholder |
| `android/` | Kotlin/Compose capture shell (`dev` / `devLocal` flavors) |
| `schemas/`, `contracts/fixtures/` | JSON Schema + golden fixtures |
| `deploy/` | Compose (local + deployed Traefik overlay) |
| `e2e/` | Playwright harness |
| `.woodpecker/` | Push CI workflows (`checks` ∥ `web` ∥ `android` → `deploy`) + PR review |

## Documentation

| Doc | Purpose |
| --- | --- |
| **[docs/PLAN.md](docs/PLAN.md)** | Product spec, stack decisions, engineering workflows |
| **[AGENTS.md](AGENTS.md)** | Agent working rules (Kanban, semver, CI, git safety) |
| **[docs/DEV.md](docs/DEV.md)** | Local development and CI reproduction |
| **[docs/DEPLOY.md](docs/DEPLOY.md)** | Woodpecker deploy, secrets, image tags |
| **[docs/PR-AGENT.md](docs/PR-AGENT.md)** | Woodpecker PR review agent |

## License

Source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE). This is **not** an OSI-approved open source license.

**Commercial use** requires a separate license. See [LICENSE-TIER.md](LICENSE-TIER.md).
