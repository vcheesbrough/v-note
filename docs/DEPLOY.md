# Deploy — v-note

**Status:** Authentik blueprint + OIDC env wiring land in **#146**; live deploy smoke in **#152**.

**Spec:** [`PLAN.md`](PLAN.md) · **Local dev:** [`DEV.md`](DEV.md) · **Reference:** [bored `.woodpecker/build.yml`](https://github.com/vcheesbrough/bored/blob/main/.woodpecker/build.yml)

---

## Environments

| Env | URL | Compose project (example) | DB volume (example) | OIDC scope |
| --- | --- | --- | --- | --- |
| **dev** | `https://v-notes-dev.desync.link` | `v-note-dev` | `v-note-dev-db` | `v-note:dev:access` |
| **prod** | `https://v-notes.desync.link` | `v-note` | `v-note-prod-db` | `v-note:prod:access` |

**Shared:** Traefik on **mini**, Authentik at **`https://auth.desync.link`**, registry **`registry.desync.link`**.

---

## Woodpecker deploy pipeline

Normal push builds automatically deploy **dev** after `e2e-web` passes. Manual deployment with **`CI_PIPELINE_DEPLOY_TARGET=dev`** or **`prod`** remains available (bored-aligned steps in `.woodpecker/build.yml`):

1. **validate-deployment** — manual deployment target is `dev` or `prod`; **prod only from `master`**
2. **compute-version** — semver from workspace + tag count (`0.N.P` pre-MVP; **`1.0.0`** after MVP **#151**)
3. **apply-authentik-blueprint** — `authentik/blueprint.yaml` to **`auth.desync.link`** before roll-out
4. **deploy** — `scripts/deploy-v-note.sh dev|prod` pulls the tested image tag and runs `docker compose` on mini (docker socket)
5. **tag-release** — after a successful dev/prod deploy, push the git tag matching `.release-tag` so the next deployment advances the patch digit

Push auto-dev deploy uses the same script and the same dev secrets as manual `deploy-dev`, but it is gated by the successful push path: `contract-validation`, `build-android`, both Android instrumented lanes (`android-instrumented-api-29` and `android-instrumented-api-36`), `build-web`, and `e2e-web` must pass before `apply-authentik-blueprint-auto-dev`, `auto-deploy-dev`, and `tag-release-auto-dev` run. Prod remains manual-only and is never deployed from a push event.

Operator reproduction from a Woodpecker-equivalent shell:

```bash
./scripts/deploy-v-note.sh dev
./scripts/deploy-v-note.sh prod
```

---

## Secrets (OpenBao)

**Never commit values.** Two OpenBao paths:

### Woodpecker mini deploy

`secret/woodpecker/repos/vcheesbrough/v-note` (broker layout, same as bored):

| Woodpecker secret key | Used for |
| --- | --- |
| `v_note_dev_oidc_client_secret` | SPA confidential client (dev) |
| `v_note_prod_oidc_client_secret` | SPA confidential client (prod) |
| `v_note_dev_postgres_password` | Postgres `POSTGRES_PASSWORD` (dev deploy) |
| `v_note_prod_postgres_password` | Postgres `POSTGRES_PASSWORD` (prod deploy) |
| `v_note_dev_sovereign_access_url` | Access URL for the `/v-note/dev/server` sovereign-config subtree |
| `v_note_prod_sovereign_access_url` | Access URL for the `/v-note/prod/server` sovereign-config subtree |
| `v_note_dev_assetlinks_json` | Minified App Links JSON, source for `android/assetlinks-json` (dev, package `link.desync.vnote.dev`) |
| `v_note_prod_assetlinks_json` | Minified App Links JSON, source for `android/assetlinks-json` (prod, package `link.desync.vnote`) |
| Android signing (dev) | Committed **non-secret** debug keystore `android/app/debug.keystore` (all builds share it → stable cert + App Links fingerprint) |
| Android signing (prod) | Secret release keystore — **outside repo**, blocker tracked in **#178** (must precede any prod Android release) |

Rotate with `bao kv patch` on mini. CI injects these via Woodpecker — **no `.env` on the host**.

### Local compose (WSL / laptop)

`secret/v-note-stack/env`:

| Key | Used for |
| --- | --- |
| `POSTGRES_PASSWORD` | Local Postgres in `deploy/docker-compose.yml` |
| `OIDC_CLIENT_SECRET` | SPA client secret (mock OIDC or Authentik) |

Fetch into gitignored `deploy/.env`: **`./scripts/fetch-compose-env.sh`** (merges with committed **`deploy/compose.env`**). Seed: **`./scripts/patch-v-note-openbao-secrets.sh`**.

---

## Runtime config (sovereign-config)

Since iteration 19 the app's runtime configuration lives in **sovereign-config**
under `/v-note/dev/server` and `/v-note/prod/server`, not in compose
env vars. The server loads four independent groups — `database`, `oidc`,
`observability`, `android` — and **refuses to start (non-zero exit, redacted
error) if any value is missing or invalid**.

The deploy step exposes the per-env access URL (Woodpecker secret
`v_note_{dev,prod}_sovereign_access_url`) as the `SOVEREIGN_CONFIG_ACCESS_URL_FILE`
env var; compose sources a docker secret of the same name straight from it and
mounts it at `/run/secrets/SOVEREIGN_CONFIG_ACCESS_URL_FILE`, which the container's
`SOVEREIGN_CONFIG_ACCESS_URL_FILE` points at. **The URL is itself a secret and
selects the environment** — dev vs prod is decided by which URL is injected, not by
a config flag.

### Creating / rotating an access URL (operator)

The access URL grants read access to the **whole** `/v-note/<env>/server`
subtree, secret leaves included — treat it like a password.

1. Create a managed connection (sovereign-config MCP or web UI), scoped and
   read-only — the URL is displayed **once**:
   `create_connection root=/v-note/dev/server permissions=["read"]`
2. Pipe it straight into OpenBao without it touching a terminal argument or
   shell history:
   ```bash
   export BAO_ADDR=https://secrets.desync.link BAO_TOKEN=<write token>
   ./scripts/store-sovereign-access-url.sh dev    # paste URL, Ctrl-D
   ./scripts/store-sovereign-access-url.sh prod
   ```
3. Redeploy. To rotate, `rotate_connection` and repeat — no app change needed.

Secret leaves (`database/password`, `oidc/client-secret`) are stored with
`put_secret` and revealed to the app at load. `POSTGRES_PASSWORD` **also** stays in
OpenBao because the `postgres` service consumes it directly — the same value lives
in two stores.

The provider is pinned to the running sovereign-config server's tag (**2.12.1**)
and **fails closed on protocol mismatch**; if that server is upgraded, bump
`sovereign-config-provider` in `crates/server/Cargo.toml` and rebuild.

### Compose env vars that remain (per deployment)

| Variable (example) | Purpose |
| --- | --- |
| `V_NOTE_HOST` | `v-notes.desync.link` vs `v-notes-dev.desync.link` |
| `V_NOTE_CONTAINER_NAME` | `v-note` vs `v-note-dev` |
| `DB_VOLUME` | `v-note-prod-db` vs `v-note-dev-db` |
| `APP_ENV` | compose-level only — `OTEL_RESOURCE_ATTRIBUTES` + `observability.env` labels |
| `APP_VERSION` | compose-level only — `OTEL_RESOURCE_ATTRIBUTES` + `observability.release` labels (the server's own `/api/meta` version is baked in at build via `V_NOTE_RELEASE`, not read here) |
| `SOVEREIGN_CONFIG_ACCESS_URL_FILE` | in-container path to the access-URL secret; blank disables the sovereign layer |

Everything else (database, OIDC, OTLP, metrics address, App Links JSON) now comes
from sovereign-config, overridable per-deploy through the `VNOTE__*` env layer
documented in [`DEV.md`](DEV.md).

**OIDC is mandatory** — the server exits non-zero at startup if `oidc/issuer-url` or
related leaves are missing; deploy reads them from sovereign-config, local compose
and e2e supply them via `VNOTE__*` (mock OIDC).

App Links JSON is sourced from the Woodpecker secrets
`v_note_{dev,prod}_assetlinks_json` and stored at the `android/assetlinks-json`
leaf. Example shape: `deploy/assetlinks.{dev,prod}.json` (documentation only — do
not commit real fingerprints).

## Observability

v-note integrates with the mini-config monitoring stack on `proxy-backend`:

- **Metrics:** the app serves Prometheus text on internal port `9090` at `/metrics`. Alloy discovers it through Docker labels on the `v-note` service: `observability.metrics.scrape=true`, `observability.metrics.port=9090`, `observability.metrics.path=/metrics`, `observability.metrics.scheme=http`, `observability.service=v-note`, `observability.env`, `observability.release`, and `observability.protocol`.
- **Traces:** the `observability` config group sets `otlp-endpoint=http://monitor-alloy:4317`, `otlp-protocol=grpc`, and `service-name=v-note` in both env subtrees; `observability/environment` supplies the OTEL `deployment.environment` attribute (`dev` / `production`).
- **Logs:** the server writes structured JSON to stdout/stderr. Docker log scraping gets environment, release, protocol, and service metadata from the same Docker labels; request IDs, user/page/session IDs, trace IDs, and error details stay in JSON log fields.
- **No public metrics route:** `/metrics` is present on the app for internal scrape and e2e checks, but should not be routed through Traefik as a public service.

**Seed Woodpecker App Links secrets** (operator, on mini or with write access to `secret/woodpecker/repos/vcheesbrough/v-note`):

```bash
export BAO_ADDR=https://secrets.desync.link
export BAO_TOKEN=<token>

# Dev — fingerprint of the committed keystore android/app/debug.keystore (all builds
# sign with it, so this is fixed): SHA-256
#   3A:49:7C:AE:57:AD:FF:E4:D0:C8:3B:D2:D0:98:2C:C2:98:CB:1D:B6:3F:70:68:5A:57:13:07:96:CC:9C:62:3A
export V_NOTE_DEV_ANDROID_CERT_SHA256="$(./scripts/android-dev-debug-fingerprint.sh)"
./scripts/patch-v-note-woodpecker-openbao-secrets.sh

# Prod — release keystore SHA-256 (keytool -list -v …), when prod Android ships:
export V_NOTE_PROD_ANDROID_CERT_SHA256='AA:BB:CC:...'
./scripts/patch-v-note-woodpecker-openbao-secrets.sh
```

Manual render (without patch script): `./scripts/render-assetlinks-json.sh dev "$SHA"` → pipe to `bao kv patch` as `v_note_dev_assetlinks_json`.

Obtain SHA-256: `./scripts/android-dev-debug-fingerprint.sh` (local debug keystore), `--docker` only for the CI image keystore, or `keytool -list -v` on a release keystore (prod).

---

## Image tags

| Phase | Dev | Prod |
| --- | --- | --- |
| Pre-MVP | `0.N.P-<sha>` | — (no prod MVP until **#151**) |
| MVP **1.0.0** | `1.0.0-<sha>` | `1.0.0` + git tag **`v1.0.0`** |
| Post-MVP | `1.N.P-<sha>` | `1.N.P` |

**Tag source:** Woodpecker **`compute-version`** → **`.release-tag`** (plain semver `MAJOR.MINOR.PATCH`, e.g. `0.3.0`).

### OCI image metadata

All four repo-built images set [OCI Image Spec](https://github.com/opencontainers/image-spec/blob/main/annotations.md) labels:

| Image | Dockerfile / compose | CI tag (examples) |
| --- | --- | --- |
| **`v-note`** | `Dockerfile.web` | `registry.desync.link/v-note:{release}` |
| **`v-note-android`** | `Dockerfile.android` | `registry.desync.link/v-note-android:{release}` |
| **`v-note-android-instrumented`** | `Dockerfile.android-instrumented` | `v-note-android-instrumented:{sha}-api{29\|36}` |
| **`v-note-e2e-playwright`** | `e2e/docker-compose.test.yml` | `v-note-e2e-playwright:{release}` |

Label sources (no `LABEL` instructions in Dockerfiles — all set at build time):

| Label | Source |
| --- | --- |
| Static (title, description, licenses, url, authors, vendor, documentation, base.name, base.digest) | **`docker build --label`** in [`.woodpecker/build.yml`](../.woodpecker/build.yml), or compose **`build.labels`** (deploy compose, e2e playwright) |
| `org.opencontainers.image.version` | `docker build --label` or compose `build.labels` (`.release-tag` / `OCI_IMAGE_VERSION`) |
| `org.opencontainers.image.revision` | `docker build --label` or compose `build.labels` (`CI_COMMIT_SHA` / `OCI_IMAGE_REVISION`) |
| `org.opencontainers.image.source` | `docker build --label` or compose `build.labels` |
| `org.opencontainers.image.created` | `docker build --label` or compose `build.labels` (UTC RFC 3339 at build time) |

Woodpecker applies these labels to every repo-built image. The **`e2e-web`** step also runs **`scripts/check-image-metadata.sh`** against the Playwright image and fails if labels are missing or the version/revision does not match.

**Build context:** each image uses a Dockerfile-paired ignore file (BuildKit convention) so `COPY . .` cache is not busted by unrelated tree changes:

| Image | Ignore file |
| --- | --- |
| Web | `Dockerfile.web.dockerignore` |
| Android | `Dockerfile.android.dockerignore` |
| Android instrumented | `Dockerfile.android-instrumented.dockerignore` |
| Playwright e2e | `e2e/.dockerignore` (compose `context: e2e/`) |

Local check after build — copy `--label` flags from `.woodpecker/build.yml` (`build-web`, `build-android`, `android-instrumented`); substitute `0.3.0-local`, `$SHA`, and `$CREATED` for version/revision/created. Example (web):

```bash
SHA=$(git rev-parse HEAD)
CREATED="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

docker build -f Dockerfile.web \
  --label org.opencontainers.image.title=v-note \
  --label "org.opencontainers.image.description=v-note server (Axum API + Leptos SPA static)" \
  --label org.opencontainers.image.licenses=PolyForm-Noncommercial-1.0.0 \
  --label org.opencontainers.image.url=https://github.com/vcheesbrough/v-note \
  --label org.opencontainers.image.authors="Vincent Cheesbrough" \
  --label org.opencontainers.image.vendor="Vincent Cheesbrough" \
  --label org.opencontainers.image.documentation=https://github.com/vcheesbrough/v-note/blob/master/docs/DEPLOY.md \
  --label org.opencontainers.image.base.name=debian:trixie-slim \
  --label org.opencontainers.image.base.digest=sha256:b6e2a152f22a40ff69d92cb397223c906017e1391a73c952b588e51af8883bf8 \
  --label org.opencontainers.image.version=0.3.0-local \
  --label org.opencontainers.image.revision="$SHA" \
  --label org.opencontainers.image.source=https://github.com/vcheesbrough/v-note \
  --label org.opencontainers.image.created="$CREATED" \
  -t v-note:local .
./scripts/check-image-metadata.sh v-note:local 0.3.0-local "$SHA"

export OCI_IMAGE_VERSION=0.3.0-local OCI_IMAGE_REVISION="$SHA" OCI_IMAGE_CREATED="$CREATED"
TEST_IMAGE=v-note:local docker compose -f e2e/docker-compose.test.yml build playwright
./scripts/check-image-metadata.sh v-note-e2e-playwright:0.3.0-local 0.3.0-local "$SHA"
```

---

## Client–server version lockstep

Server and SPA ship in the **same image** (same **`release`**). **Android is sideloaded** — it can drift after a server-only deploy.

**Operator default (all phases):** deploy image tag **`X.Y.Z`** **and** install the **APK built for the same `X.Y.Z`** (same CI run or git tag). **Rollback:** previous image tag **and** matching APK.

**Runtime enforcement** (see [`PLAN.md`](PLAN.md) **Client–server version alignment**):

| Phase | Rule |
| --- | --- |
| **Pre-MVP** (`0.N.P`) | **Strict** — client **`release`** must exactly match server |
| **MVP+** (`1.N.P`) | **Relaxed** — same **`major.minor`** OK; patch may drift; cross-minor rejected |

Clients send **`X-V-Note-Client-Release`** / **`X-V-Note-Client-Protocol`**; **`protocol`** must always match.

## Rollback

Redeploy a **previous image tag** via Woodpecker manual deploy with pinned version env (detail in **#152** runbook). **Also reinstall the matching Android APK.**

---

## Android APK sideload (dev)

After a successful dev deploy, the `dev` APK is served at:

```
https://v-notes-dev.desync.link/dl/apk
```

The canonical URL remains stable. SPA links add `?release={release}` and the same versioned filename in their `download` attribute, giving Android Chrome a distinct download identity before the response arrives. Nginx ignores the query for route matching and serves the same `/dl/apk` resource. Served by `registry.desync.link/v-note-android:{release}` (nginx:alpine) via `deploy/docker-compose.android-apk.yml`, Traefik `Host + Path(/dl/apk)` rule. The response also returns `Content-Disposition: attachment; filename="v-note-{release}-dev-debug.apk"` (for example, `v-note-0.13.1-dev-debug.apk`).

Download on the device browser and enable "Install from unknown sources", or:

```bash
curl -L -OJ https://v-notes-dev.desync.link/dl/apk
adb install v-note-{release}-dev-debug.apk
```

The `dev` flavor connects to `https://v-notes-dev.desync.link` — no `adb reverse` needed. For laptop dev with a local server, use the `devLocal` APK (see [`DEV.md`](DEV.md)).

---

## Post-deploy smoke

- **#145:** pipeline structure only; optional manual curl
- **#152:** automated live smoke (TLS, Authentik, realtime, CSP)
