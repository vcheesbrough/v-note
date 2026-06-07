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

Exact names in `deploy/docker-compose.yml`. Android App Links templates: `deploy/assetlinks.dev.json`, `deploy/assetlinks.prod.json` (or `deploy/assetlinks.example.json` for dev). Woodpecker `deploy-dev` / `deploy-prod` inject secrets `v_note_dev_assetlinks_json` / `v_note_prod_assetlinks_json` into the step environment; `docker compose` reads `ASSETLINKS_JSON` from that env (not inlined in the deploy script — JSON quoting is unsafe in shell).

**Seed Woodpecker secrets** (operator, on mini):

```bash
# Dev — edit deploy/assetlinks.dev.json with the devDebug (or release) cert SHA-256, then:
bao kv patch secret/woodpecker/repos/vcheesbrough/v-note \
  v_note_dev_assetlinks_json="$(jq -c . deploy/assetlinks.dev.json)"

# Prod — edit deploy/assetlinks.prod.json with the release keystore SHA-256, then:
bao kv patch secret/woodpecker/repos/vcheesbrough/v-note \
  v_note_prod_assetlinks_json="$(jq -c . deploy/assetlinks.prod.json)"
```

Example minified value (dev):

```json
[{"relation":["delegate_permission/common.handle_all_urls"],"target":{"namespace":"android_app","package_name":"link.desync.vnote.dev","sha256_cert_fingerprints":["AA:BB:CC:..."]}}]
```

Obtain SHA-256: `keytool -list -v -keystore <keystore> -alias <alias>` (release) or debug keystore for dev sideload builds.

---

## Image tags

| Phase | Dev | Prod |
| --- | --- | --- |
| Pre-MVP | `0.N.P-<sha>` | — (no prod MVP until **#151**) |
| MVP **1.0.0** | `1.0.0-<sha>` | `1.0.0` + git tag **`v1.0.0`** |
| Post-MVP | `1.N.P-<sha>` | `1.N.P` |

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
