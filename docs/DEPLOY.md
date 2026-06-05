# Deploy — v-note

**Status:** Pipeline skeleton lands in **#145**; live deploy smoke in **#152**.

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

## Secrets (OpenBao → Woodpecker)

Document keys here as they are wired in **#145** — **never commit values**.

| Secret (example name) | Used for |
| --- | --- |
| `OIDC_CLIENT_SECRET_DEV` | SPA confidential client (dev) |
| `OIDC_CLIENT_SECRET_PROD` | SPA confidential client (prod) |
| `DATABASE_URL` / per-env DB creds | Postgres (if not inline in compose) |
| Android signing (post-MVP prod) | Release keystore — **outside repo** |

Mirror bored OpenBao layout where applicable.

---

## Compose env vars (per deployment)

| Variable (example) | Purpose |
| --- | --- |
| `V_NOTE_HOST` | `v-notes.desync.link` vs `v-notes-dev.desync.link` |
| `DB_VOLUME` | `v-note-prod-db` vs `v-note-dev-db` |
| `REQUIRED_SCOPE` | `v-note:prod:access` vs `v-note:dev:access` |
| `OIDC_ISSUER_URL` | Matching Authentik provider issuer |

Exact names frozen in **#145** `deploy/docker-compose.yml`.

---

## Image tags

| Phase | Dev | Prod |
| --- | --- | --- |
| Pre-MVP | `0.N.P-<sha>` | — (no prod MVP until **#151**) |
| MVP **1.0.0** | `1.0.0-<sha>` | `1.0.0` + git tag **`v1.0.0`** |
| Post-MVP | `1.N.P-<sha>` | `1.N.P` |

---

## Rollback

Redeploy a **previous image tag** via Woodpecker manual deploy with pinned version env (detail in **#152** runbook).

---

## Post-deploy smoke

- **#145:** pipeline structure only; optional manual curl
- **#152:** automated live smoke (TLS, Authentik, realtime, CSP)
