# Local development — v-note

**Status:** Scaffold lands in **#145** — commands below are **targets** until `justfile` / compose exist.

**Spec:** [`PLAN.md`](PLAN.md) · **Engineering workflows** · **Agent rules:** [`AGENTS.md`](../AGENTS.md)

---

## Prerequisites

| Tool | Purpose |
| --- | --- |
| **Rust** (stable) | Server, worker, `crates/protocol` |
| **Docker** + Compose | Local stack, e2e reproduction |
| **Trunk** | Leptos SPA (`frontend/`) |
| **Android SDK** + emulator | Kotlin app, instrumented e2e |
| **Node.js** | Playwright (`e2e/`) |
| **just** (or Make) | Convenience targets — optional until **#145** |
| **NetBird / LAN** | Reach **`v-notes-dev.desync.link`** when testing against deployed dev (not required for local compose) |

---

## Quick start (after **#145**)

```bash
# Full local stack (server + postgres + SPA)
just run-compose
# or: docker compose -f deploy/docker-compose.yml up

# Individual artifacts
just run-server    # cargo run -p server
just run-spa       # trunk serve --directory frontend
just build-android # ./gradlew :app:assembleDevDebug

# E2e (same gate as CI)
just e2e
# or: TEST_IMAGE=v-note:ci-local docker compose -f e2e/docker-compose.test.yml up --exit-code-from playwright
```

---

## CI reproduction

When Woodpecker fails after push:

```bash
docker build -t v-note:ci-local .
TEST_IMAGE=v-note:ci-local docker compose -f e2e/docker-compose.test.yml up \
  --build --force-recreate --abort-on-container-exit --exit-code-from playwright
```

Verify GitHub status:

```bash
SHA=$(git rev-parse HEAD)
gh api repos/vcheesbrough/v-note/commits/$SHA/status --jq '.state'
```

See [`AGENTS.md`](../AGENTS.md) §3.

---

## Database migrations (local)

```bash
# After sqlx + compose postgres exist
just migrate
# or: sqlx migrate run (with DATABASE_URL from compose)
```

**Forward-only** — see [`PLAN.md`](PLAN.md) **Engineering workflows** → **Database migrations**.

---

## Versioning on feature branches

| Phase | Example |
| --- | --- |
| Pre-MVP | `0.N.0` in root `Cargo.toml` — e.g. first iteration **`0.1.0`** (likely **#145** if started first) |
| MVP release | **`1.0.0`** on **`main`**, tag **`v1.0.0`** when MVP completion card merges (today **#151**) |
| Post-MVP | `1.N.0` — **`N` continues** globally (not a fixed offset from card numbers) |

Iteration **`N`** and branch `feat/iteration-N-slug` — [`AGENTS.md`](../AGENTS.md) §1.
