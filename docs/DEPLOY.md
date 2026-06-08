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

Triggered manually with **`CI_PIPELINE_DEPLOY_TARGET=dev`** or **`prod`** (bored-aligned steps in `.woodpecker/build.yml`):

1. **validate-deployment** — target is `dev` or `prod`; **prod only from `main`**
2. **compute-version** — semver from workspace + tag count (`0.N.P` pre-MVP; **`1.0.0`** after MVP **#151**)
3. **apply-authentik-blueprint** — `authentik/blueprint.yaml` to **`auth.desync.link`** before roll-out
4. **push** — `registry.desync.link/v-note:{version}` and `:{sha}` (dev)
5. **deploy** — `docker compose -f deploy/docker-compose.yml up -d --pull always` on mini (docker socket)

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
| `v_note_dev_assetlinks_json` | Minified JSON for `ASSETLINKS_JSON` (dev App Links, package `link.desync.vnote.dev`) |
| `v_note_prod_assetlinks_json` | Minified JSON for `ASSETLINKS_JSON` (prod App Links, package `link.desync.vnote`) |
| Android signing (post-MVP prod) | Release keystore — **outside repo** |

Rotate with `bao kv patch` on mini. CI injects these via Woodpecker — **no `.env` on the host**.

### Local compose (WSL / laptop)

`secret/v-note-stack/env`:

| Key | Used for |
| --- | --- |
| `POSTGRES_PASSWORD` | Local Postgres in `deploy/docker-compose.yml` |
| `OIDC_CLIENT_SECRET` | SPA client secret (mock OIDC or Authentik) |

Fetch into gitignored `deploy/.env`: **`./scripts/fetch-compose-env.sh`** (merges with committed **`deploy/compose.env`**). Seed: **`./scripts/patch-v-note-openbao-secrets.sh`**.

---

## Compose env vars (per deployment)

| Variable (example) | Purpose |
| --- | --- |
| `V_NOTE_HOST` | `v-notes.desync.link` vs `v-notes-dev.desync.link` |
| `DB_VOLUME` | `v-note-prod-db` vs `v-note-dev-db` |
| `REQUIRED_SCOPE` | `v-note:prod:access` vs `v-note:dev:access` |
| `OIDC_ISSUER_URL` | Matching Authentik provider issuer |
| `OIDC_CLIENT_ID` | SPA confidential client (`v-note-browser-{dev,prod}`) |
| `OIDC_CLIENT_SECRET` | SPA client secret (Woodpecker secret per env) |
| `OIDC_REDIRECT_URI` | `https://{host}/auth/callback` |
| `OIDC_END_SESSION_URL` | Authentik RP logout URL for env |
| `OIDC_ANDROID_CLIENT_ID` | Android app client (`v-note-android-{dev,prod}`) |
| `OIDC_ANDROID_ISSUER_URL` | Android Authentik provider issuer URL |
| `ASSETLINKS_JSON` | JSON served at `/.well-known/assetlinks.json` for Android App Links |

**OIDC is mandatory** — the server panics at startup if `OIDC_ISSUER_URL` or related vars are missing; deploy and local compose always set them (Authentik on mini, mock OIDC locally).

Exact names in `deploy/docker-compose.yml`. **`ASSETLINKS_JSON` is required for deploy** — Woodpecker injects minified JSON from OpenBao keys `v_note_dev_assetlinks_json` / `v_note_prod_assetlinks_json` (step `environment:` → `docker compose` reads `${ASSETLINKS_JSON}`). Example shape: `deploy/assetlinks.{dev,prod}.json` (documentation only — do not commit real fingerprints).

**Seed Woodpecker App Links secrets** (operator, on mini or with write access to `secret/woodpecker/repos/vcheesbrough/v-note`):

```bash
export BAO_ADDR=https://secrets.desync.link
export BAO_TOKEN=<token>

# Dev — fingerprint from your local debug keystore (fast; must match the APK you sideload):
export V_NOTE_DEV_ANDROID_CERT_SHA256="$(./scripts/android-dev-debug-fingerprint.sh)"
# CI container keystore instead (slow): ./scripts/android-dev-debug-fingerprint.sh --docker
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
| **`v-note-android`** | `Dockerfile.android` | `v-note-android:{sha}` |
| **`v-note-android-instrumented`** | `Dockerfile.android-instrumented` | `v-note-android-instrumented:{sha}` |
| **`v-note-e2e-playwright`** | `e2e/docker-compose.test.yml` | `v-note-e2e-playwright:{release}` |

Label sources (no `LABEL` instructions in Dockerfiles — all set at build time):

| Label | Source |
| --- | --- |
| Static (title, description, licenses, url, authors, vendor, documentation, base.name, base.digest) | **`docker build --label`** in [`.woodpecker/build.yml`](../.woodpecker/build.yml), or compose **`build.labels`** (deploy compose, e2e playwright) |
| `org.opencontainers.image.version` | `docker build --label` or compose `build.labels` (`.release-tag` / `OCI_IMAGE_VERSION`) |
| `org.opencontainers.image.revision` | `docker build --label` or compose `build.labels` (`CI_COMMIT_SHA` / `OCI_IMAGE_REVISION`) |
| `org.opencontainers.image.source` | `docker build --label` or compose `build.labels` |
| `org.opencontainers.image.created` | `docker build --label` or compose `build.labels` (UTC RFC 3339 at build time) |

Woodpecker runs **`scripts/check-image-metadata.sh`** after **`build-web`** (before push), **`build-android`**, **`android-instrumented`**, and **`e2e-web`** (playwright build) — pipeline fails if labels are missing or version/revision mismatch.

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

## Post-deploy smoke

- **#145:** pipeline structure only; optional manual curl
- **#152:** automated live smoke (TLS, Authentik, realtime, CSP)
