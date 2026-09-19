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

Normal push builds automatically deploy **dev** once every push workflow passes. Manual deployment with **`CI_PIPELINE_DEPLOY_TARGET=dev`** or **`prod`** remains available (bored-aligned steps in `.woodpecker/deploy.yml`):

1. **validate-deployment** — manual deployment target is `dev` or `prod`; **prod only from `master`**
2. **compute-version** — semver from workspace + tag count (`0.N.P` pre-MVP; **`1.0.0`** after MVP **#151**)
3. **apply-authentik-blueprint** — `authentik/blueprint.yaml` to **`auth.desync.link`** before roll-out
4. **deploy** — `scripts/deploy-v-note.sh` pulls the tested image tag and runs `docker compose` on mini (docker socket), then **gates on health**: it polls the container's own healthcheck status (`HEALTHCHECK` in [`Dockerfile.web`](../Dockerfile.web), which curls `https://127.0.0.1:443/health`) and fails the deploy if it never reports healthy. `docker compose up -d` alone only proves the container was *created* — a crash-looping container would otherwise report a green deploy. Gating on the container's own status rather than a separate probe means the deploy passes on exactly the condition `docker ps` reports, and both failure modes are *decided* rather than waited out: a process that dies on bad config is caught by its **run state** (`exited` / `restarting`) in seconds — it never reports unhealthy at all, which is precisely why the old probe burned the full timeout on every crash loop — and `unhealthy` is **terminal**, because docker has already applied the configured retries. The 120s deadline now only covers an app that stays up and never finishes starting. The failure dump includes `.State.Health.Log`, i.e. the last five probe attempts with curl's own error text. **Rolling back to an image built before iteration 23** has no healthcheck to gate on; the script says so explicitly rather than polling until the deadline.
5. **tag-release** — after a successful dev/prod deploy, push the git tag matching `.release-tag` so the next deployment advances the patch digit
6. **publish-grafana-dashboard** — **every push, on any branch**, after `auto-deploy-dev` and alongside `tag-release-auto-dev` (it does not gate it): publishes `deploy/grafana/v-note-overview.json` to Grafana — see [Grafana dashboard](#grafana-dashboard)

Push auto-dev deploy uses the same script and literally the same environment block as manual `deploy-dev` (a YAML anchor, so they cannot drift), but it is gated by the successful push path. The gate is the **workflow-level** `depends_on` of `deploy.yml`: the `checks` workflow (`lint`, `rust-test`, `deploy-script-validation`, `grafana-dashboard-validation`, `android-build-box-pin`), the `web` workflow (`build-web`, `e2e-web`) and the `android` workflow (`build-android` and both instrumented lanes, `android-instrumented-api-29` / `-36`) must all succeed before `deploy.yml` starts at all. The dependencies are marked `optional` only so that a manual deployment — which runs none of those workflows — is not blocked; on a push all three are present and enforced. Prod remains manual-only and is never deployed from a push event.

Because workflows share nothing, `deploy.yml` computes the release tag again. The first push step, **verify-release-images**, pulls `v-note:{release}` and `v-note-android:{release}` and fails unless both carry this commit's `org.opencontainers.image.revision` — so a tag pushed by another pipeline in between can never roll out someone else's images.

### The deploy script takes no arguments

`scripts/deploy-v-note.sh` has **one entry point and no modes**. It does not know
that dev and prod exist: every environment-specific value is a parameter set by
the calling step in [`.woodpecker/deploy.yml`](../.woodpecker/deploy.yml), where
the dev block is defined once and reused by `auto-deploy-dev` via a YAML anchor.
Adding an environment means adding a step, not editing the script.

| Parameter | Purpose |
| --- | --- |
| `REGISTRY_USER` / `REGISTRY_PASSWORD` | `registry.desync.link` credentials |
| `POSTGRES_PASSWORD` | consumed directly by the `postgres` service |
| `SOVEREIGN_CONFIG_ACCESS_URL_FILE` | unlocks the app's sovereign-config subtree; **which URL is injected selects the environment** |
| `V_NOTE_METRICS_ADDR` | `host:port` or `disabled` — see the broker alias above |
| `COMPOSE_PROJECT_NAME` | compose project; read by `docker compose` itself, so the script passes no `-p` |
| `COMPOSE_FILE` | `:`-separated compose files; likewise no `-f` |
| `V_NOTE_IMAGE_REPOS` | space-separated repos to pull at the release tag (dev adds `v-note-android`) |
| `V_NOTE_HOST`, `V_NOTE_CONTAINER_NAME`, `DB_VOLUME`, `APP_ENV` | passed straight through to compose |

All of these are required. The script explicitly checks only the four whose
absence would otherwise be **silent** — `APP_ENV` (compose falls back to `dev`,
so a prod deploy would label itself `env=dev`), `COMPOSE_PROJECT_NAME` (compose
falls back to the compose file's directory name, deploying into a parallel
project), `SOVEREIGN_CONFIG_ACCESS_URL_FILE` (compose accepts a blank secret and
the app starts with no runtime config), and `V_NOTE_IMAGE_REPOS` (the pull loop
does nothing). The rest already fail loudly on their own: `set -u` catches any
unset variable the script dereferences, `POSTGRES_PASSWORD` / `V_NOTE_HOST` /
`V_NOTE_CONTAINER_NAME` / `DB_VOLUME` carry `:?` guards in
`deploy/docker-compose.yml`, empty registry credentials fail `docker login`, and
a malformed or empty `V_NOTE_METRICS_ADDR` is rejected by the script's own
`case`. `V_NOTE_IMAGE_TAG` (overrides `.release-tag`) and `DOCKER_NETWORK` are
optional.

**Nothing with a side effect runs until every parameter has been checked.** The
compose `:?` guards would otherwise not fire until `up` — after a registry login
and an image pull — so the script resolves the model first with `docker compose
config --quiet`. That is client-side only (no daemon, no network) and also
catches a missing or wrong `COMPOSE_FILE` and any schema error in the compose
files themselves.

What the script keeps is the shell that is awkward to inline into a Woodpecker
`commands:` block: the metrics-addr → scrape-label derivation (one value with two
consumers — passing it in pre-split would reintroduce the drift it exists to
prevent), the explicit recreate and its rationale, the health gate, and the
parameter checks — the four-name guard and the compose pre-flight. It is also the
only form of the deploy that can be run by hand or checked outside CI.

Those checks are exercised by
[`scripts/test-deploy-v-note.sh`](../scripts/test-deploy-v-note.sh), run in the
`deploy-script-validation` pipeline step. It puts a stub `docker` on `PATH` and
asserts each bad input fails *with no docker call at all*, plus that the scrape
labels track the listener port, that the recreate still happens, and that the
health gate behaves on each state the container can report (`starting` is waited
out; `unhealthy`, a crash loop, and an image with no healthcheck each fail the
deploy with the reason) — the deploy steps themselves only ever exercise the happy path. It runs on the docker CLI
image because the pre-flight cases need the real compose plugin to fire the `:?`
guards, but needs no docker socket: `compose config` resolves the model
client-side.

Operator reproduction from a Woodpecker-equivalent shell — export the parameters
for the environment you want, exactly as the pipeline step sets them, then:

```bash
export COMPOSE_PROJECT_NAME=v-note-dev
export COMPOSE_FILE=deploy/docker-compose.yml:deploy/docker-compose.android-apk.yml
export V_NOTE_IMAGE_REPOS="registry.desync.link/v-note registry.desync.link/v-note-android"
export V_NOTE_HOST=v-notes-dev.desync.link V_NOTE_CONTAINER_NAME=v-note-dev
export DB_VOLUME=v-note-dev-db APP_ENV=dev
export REGISTRY_USER=… REGISTRY_PASSWORD=… POSTGRES_PASSWORD=…
export SOVEREIGN_CONFIG_ACCESS_URL_FILE=… V_NOTE_METRICS_ADDR=0.0.0.0:9090
./scripts/deploy-v-note.sh
```

> **Watch out on a developer box.** `COMPOSE_FILE=deploy/…` makes `deploy/` the
> compose *project directory*, and compose auto-loads `deploy/.env` from there.
> That file is gitignored and absent from CI's fresh clone, so the pipeline is
> unaffected — but locally `scripts/fetch-compose-env.sh` fills it with the
> **local** stack's values (`V_NOTE_CONTAINER_NAME=v-note-local`,
> `V_NOTE_HOST=localhost`, `DB_VOLUME=v-note-local-db`, `V_NOTE_IMAGE_TAG=local`).
> A hand-run that forgets one of the exports above silently picks those up instead
> of failing. Run the deploy from a clean checkout, or pass
> `--env-file /dev/null`, if you need the guards to behave as they do in CI.

---

## Secrets

**Never commit values.** CI secrets come from sovereign-config through the
Woodpecker broker; local compose secrets still come from OpenBao.

### Woodpecker mini deploy (sovereign-config broker)

Mini points `WOODPECKER_SECRET_EXTENSION_ENDPOINT` at the
`sovereign-config-woodpecker-broker`, whose layers are
`/woodpecker/shared,/woodpecker/repos/{repo.owner}/{repo.name}` — so a
`from_secret: <name>` in any [`.woodpecker/`](../.woodpecker/) workflow
resolves at `/woodpecker/repos/vcheesbrough/v-note/<name>`:

| Woodpecker secret key | Used for |
| --- | --- |
| `v_note_dev_postgres_password` | Postgres `POSTGRES_PASSWORD` (dev deploy) |
| `v_note_prod_postgres_password` | Postgres `POSTGRES_PASSWORD` (prod deploy) |
| `v_note_dev_sovereign_access_url` | Access URL for the `/v-note/dev/server` sovereign-config subtree |
| `v_note_prod_sovereign_access_url` | Access URL for the `/v-note/prod/server` sovereign-config subtree |
| `v_note_dev_metrics_addr` | **Alias** of `/v-note/dev/server/observability/metrics-addr` — see below |
| `v_note_prod_metrics_addr` | **Alias** of `/v-note/prod/server/observability/metrics-addr` |
| Android signing (dev) | Committed **non-secret** debug keystore `android/app/debug.keystore` (all builds share it → stable cert + App Links fingerprint) |

### Retiring the OIDC client secret (#274)

The unified client is **public + PKCE**, so no client secret is used anywhere.
Removing the *references* does not remove the *values* — the old secret still
exists in three places and should be deleted once the migration is confirmed:

1. Woodpecker secrets `v_note_dev_oidc_client_secret` and
   `v_note_prod_oidc_client_secret` (no longer read by any step).
2. OpenBao `secret/v-note-stack/env` → key `OIDC_CLIENT_SECRET`.
3. sovereign-config leaves `/v-note/{dev,prod}/server/oidc/client-secret`.

Keep them until prod is running the unified client: they are what a rollback to
the pre-#274 image would need, since that image refuses to start without a
non-empty `oidc/client-secret`. The sovereign-config leaves for
`oidc/android/client-id` and `oidc/android/issuer-url` are likewise inert and
can go at the same time.

### Migrating an environment to the unified client (#274)

The blueprint **renames** the provider (`v-note-browser-{env}` → `v-note-{env}`),
which changes the `client_id` the server must send **and** the `aud` on every
token. The server reads that from sovereign-config, so applying the blueprint
without updating the leaf breaks login in that environment:

```
/v-note/<env>/server/oidc/client-id   v-note-browser-<env>  →  v-note-<env>
```

`issuer-url` and `end-session-url` already point at the **application** slug
(`/application/o/v-note-<env>/`), which is unchanged, so they need no edit.

**Order matters:** update the leaf, then apply the blueprint, then recreate the
app container (config is read once at startup — `deploy-v-note.sh` force-recreates,
so a redeploy is sufficient). Applying the blueprint first leaves the environment
unable to authenticate until the leaf catches up.

> `apply-authentik-blueprint-auto-dev` is restricted to `branch: master`. It
> applies to the **shared** Authentik and the blueprint declares the **prod**
> objects too, so an unrestricted push would let any feature branch rewrite
> prod's provider.
| Android signing (prod) | Secret release keystore — **outside repo**, blocker tracked in **#178** (must precede any prod Android release) |

Rotate by rewriting the leaf in sovereign-config (`put_secret` / the CLI) at
`/woodpecker/repos/vcheesbrough/v-note/<name>`. CI injects these via Woodpecker
— **no `.env` on the host**.

**The two `*_metrics_addr` entries are aliases, not copies.** `AddValuePath`
exposes one stored value at several canonical paths, so the pipeline and the app
read the *same* leaf: the app resolves `observability/metrics-addr` through its
own sovereign-config client, and `deploy-v-note.sh` reads the alias to derive the
Alloy `observability.metrics.port` / `.scrape` container labels, which docker
writes at container-create time and nothing inside the container can influence.
Create them with:

```bash
sovereign-config alias add /v-note/dev/server/observability/metrics-addr \
  /woodpecker/repos/vcheesbrough/v-note/v_note_dev_metrics_addr
sovereign-config alias add /v-note/prod/server/observability/metrics-addr \
  /woodpecker/repos/vcheesbrough/v-note/v_note_prod_metrics_addr
```

This aliases *into* `/woodpecker/...`, the opposite direction to the broker
README's advice. That advice is about repository-independent values whose natural
home is the broker root; this value's canonical home is the app subtree, so the
alias points the other way. Aliasing widens read access — every v-note pipeline
can read it — which is immaterial here because it is a plain leaf, not a secret.

App Links JSON is **not** in this list: since iteration 19 it lives in sovereign-config
at `android/assetlinks-json`. The former `v_note_{dev,prod}_assetlinks_json` keys have
been deleted — rotating a signing certificate means rewriting that leaf (see
[Set App Links JSON](#set-app-links-json) below), not patching a CI secret.

### Local compose (WSL / laptop) — OpenBao

`secret/v-note-stack/env`:

| Key | Used for |
| --- | --- |
| `POSTGRES_PASSWORD` | Local Postgres in `deploy/docker-compose.yml` |

There is no OIDC client secret: the SPA and Android share one **public** Authentik
client using Authorization Code + **PKCE** (**#274**).

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

The secret leaf (`database/password`) is stored with `put_secret` and revealed to
the app at load. `POSTGRES_PASSWORD` **also** stays in
OpenBao because the `postgres` service consumes it directly — the same value lives
in two stores.

The provider is pinned to the running sovereign-config server's tag (**2.19.4**).
Only the **protocol** is enforced: the provider **fails closed on protocol
mismatch** (both sides speak `v3`), but nothing checks the tag itself — the server
once ran 2.19.4 against a 2.12.1 client without anything failing. So the pin is a
convention to keep, not a guarantee: before relocking, compare it with the live
version (sovereign-config MCP `status`, or the unauthenticated gRPC
`System.GetVersion`), and if the server has moved, bump `sovereign-config-provider`
in `crates/server/Cargo.toml` and rebuild.

### Compose env vars that remain (per deployment)

| Variable (example) | Purpose |
| --- | --- |
| `V_NOTE_HOST` | `v-notes.desync.link` vs `v-notes-dev.desync.link` — **compose-level only**, for the Traefik router rules; the container is not given it |
| `V_NOTE_CONTAINER_NAME` | `v-note` vs `v-note-dev` |
| `DB_VOLUME` | `v-note-prod-db` vs `v-note-dev-db` |
| `APP_ENV` | compose-level only — the `observability.env` discovery label |
| `APP_VERSION` | compose-level only — the `observability.release` discovery label (the server's own `/api/meta` version is baked in at build via `V_NOTE_RELEASE`, not read here) |
| `SOVEREIGN_CONFIG_ACCESS_URL_FILE` | in-container path to the access-URL secret; blank disables the sovereign layer |
| `V_NOTE_METRICS_ADDR` | compose-level only — `host:port` or `disabled`, supplied by the `v_note_{dev,prod}_metrics_addr` broker alias of `observability/metrics-addr`. `deploy-v-note.sh` derives `observability.metrics.port` and `observability.metrics.scrape` from it, so the listener and the thing scraping it read one value. **Required** — a missing value fails the deploy rather than defaulting |

The **container's only environment variable is `SOVEREIGN_CONFIG_ACCESS_URL_FILE`.**
Everything else (database, OIDC, OTLP, metrics address, App Links JSON) comes
from sovereign-config, overridable per-deploy through the `VNOTE__*` env layer
documented in [`DEV.md`](DEV.md) — a layer the deploy path deliberately no longer
uses.

There is deliberately **no `observability.protocol` label**. The protocol version
is a compile-time constant, not configuration; the compose copy sat at `2` while
the constant moved to `5`, and Alloy's relabelling pushed that stale value onto
every v-note series. `v_note_build_info{protocol=…}` now states it from the same
constant the binary uses. To filter other series by protocol, join:
`… * on(instance) group_left(protocol) v_note_build_info`.

**OIDC is mandatory** — the server exits non-zero at startup if `oidc/issuer-url` or
related leaves are missing; deploy reads them from sovereign-config, local compose
and e2e supply them via `VNOTE__*` (mock OIDC).

App Links JSON lives at the `android/assetlinks-json` leaf in sovereign-config and
is rendered from the signing certificate fingerprint — see [Set App Links JSON](#set-app-links-json)
above. Example shape: `deploy/assetlinks.{dev,prod}.json` (documentation only — do
not commit real fingerprints).

## Observability

v-note integrates with the mini-config monitoring stack on `proxy-backend`:

- **Metrics:** the app serves Prometheus text on internal port `9090` at `/metrics`. Alloy discovers it through Docker labels on the `v-note` service: `observability.metrics.scrape=true`, `observability.metrics.port=9090`, `observability.metrics.path=/metrics`, `observability.metrics.scheme=http`, `observability.service=v-note`, `observability.env`, `observability.release`, and `observability.protocol`.
- **Traces:** the `observability` config group sets `otlp-endpoint=http://monitor-alloy:4317`, `otlp-protocol=grpc`, and `service-name=v-note` in both env subtrees; `observability/environment` supplies the OTEL `deployment.environment` attribute (`dev` / `production`). Every `http.request` span adopts the W3C `traceparent` Traefik forwards, so a request's trace starts at Traefik's edge span and drills down into v-note. WebSocket connection spans nest under their upgrade request; each inbound page message is its own trace, linked to its connection span. A broadcast carries its publisher's trace context, so each socket's send of a fanned-out message is a `realtime.fanout.deliver` span (`channel`, `message_type`, `bytes`, plus `session_id` on the page channel) inside the publisher's trace — a `commit-batch` trace shows the delivery to every sibling session — and linked to the receiving connection. Every Postgres round trip — each query, and each transaction's `BEGIN`/`COMMIT` — is a `db.query` span carrying `db.operation` and `db.query_name` (the call site, never SQL text or values), plus the size of the result: `db.response.returned_rows`, `db.response.bytes` and `db.response.max_row_bytes` (Postgres wire bytes of the returned column values, in total and for the widest row), and `db.response.affected_rows` for writes. Every span also carries the OpenTelemetry `code.file.path` / `code.module.name` / `code.line.number` the tracing layer derives from where the span was opened; `db_query_span!` is a macro so that those name the query's own call site rather than `observability.rs` (#343). Thumbnail jobs run detached, so each is its own `thumbnail.generate` trace (with `db.query` and `thumbnail.render` children) linked to the request that queued it.
- **Logs:** the server writes structured JSON to stdout/stderr. Docker log scraping gets environment, release, protocol, and service metadata from the same Docker labels; request IDs, user/page/session IDs, trace IDs, and error details stay in JSON log fields.
- **No public metrics route:** `/metrics` is present on the app for internal scrape and e2e checks, but should not be routed through Traefik as a public service.

### Realtime metrics and spans

The WebSocket channels carry all the ink traffic, so they get size and latency, not just event counts:

| Metric | Type | Labels | What it measures |
| --- | --- | --- | --- |
| `v_note_realtime_message_bytes` | histogram | `channel`, `message_type` | serialized size of every frame sent (page and library channels) |
| `v_note_realtime_replay_bytes` / `_frames` | histogram | — | one observation per completed `Subscribe` replay: every `stroke-batch` + `tombstone-batch` + the closing `synced` |
| `v_note_realtime_replay_duration_seconds` | histogram | — | `Subscribe` received → `synced` sent |
| `v_note_realtime_message_handling_seconds` | histogram | `message_type` | server-side handling of one inbound page message. **Not end-to-end freshness** (that needs client timestamps — #154) |
| `v_note_realtime_events_total` | counter | `channel`, `result` | now includes `lagged`: a subscriber fell behind its broadcast channel (page skips the dropped fan-out, library closes; recovery is #279) |

`message_type` is the frame's serde `type` tag, derived by an exhaustive `match` in `crates/protocol`, so the label set is bounded by the protocol enums. **Cardinality rule:** no metric is ever labelled by `page_id`, `session_id`, `owner_id` or `client_batch_id`. `crates/server/tests/health.rs` scrapes `/metrics` and fails if any series carries one.

Those ids live in **spans** instead. The socket handlers are instrumented (`page_id`, `session_id`), and every inbound page message is its **own trace root** (`handle_page_client_message`, with `message_type`), linked to its connection span rather than nested under it. A connection lasts for hours, so a trace rooted there would not be complete in Tempo until the socket closed.

### Grafana dashboard

**`v-note — overview`** (uid **`v-note-overview`**) lives in Grafana under **Applications / v-note** (folder uid `v-note`). Its source of truth is [`deploy/grafana/v-note-overview.json`](../deploy/grafana/v-note-overview.json): application dashboards ship in the application repo, in the same PR as the metrics they chart.

- **One dashboard, both environments:** an `env` variable (`label_values(v_note_realtime_active_connections, env)`) filters every query. Prometheus is referenced by uid `PBFA97CFB590B2093`, Loki by `P8E80F9AEF21F6940`. A dashboard link opens a Tempo TraceQL search for the selected env.
- **Published by CI:** the `publish-grafana-dashboard` step in `.woodpecker/deploy.yml` runs [`scripts/publish-grafana-dashboard.sh`](../scripts/publish-grafana-dashboard.sh) on **every push, on any branch**, after `auto-deploy-dev`, so the dashboard always matches what is deployed to dev. It posts `{dashboard (id: null), folderUid, overwrite: true, message: "v-note <branch> <release> <sha>"}` to `/api/dashboards/db` with the shared `grafana_api_token` (`woodpecker-ci` service account, Edit on the Applications folder). So every entry in the dashboard's version history names its branch and commit. A non-2xx fails the step and prints Grafana's response body; the token is never printed. The last push wins, as it does for dev itself: a feature-branch push replaces the shared dashboard until the next push. `deploy-prod` never publishes, because it could roll the dashboard back to an older release.
- **UI edits are overwritten** on the next push that deploys dev. To change the dashboard, edit it in Grafana (a scratch copy is fine), export the JSON into the repo file, and keep `uid: v-note-overview` with no numeric `id`.
- **Offline validation:** [`scripts/test-grafana-dashboard.sh`](../scripts/test-grafana-dashboard.sh), run in the `checks` step `grafana-dashboard-validation`, checks that the JSON parses, keeps its uid, has no committed id, filters every query by `env`, references no unbounded id, and charts only metrics `observability.rs` registers. It also checks the publish script's `--dry-run` payload, its input guards, and its live path against a stub `curl` (2xx passes; non-2xx and transport failures fail without leaking the token).
- **Pre-merge check:** pushing a branch publishes its dashboard, so once that push's `deploy` workflow is green, check the panels under Applications / v-note against dev and record the check in the PR. `./scripts/publish-grafana-dashboard.sh --dry-run deploy/grafana/v-note-overview.json` (with `GRAFANA_FOLDER_UID`, `RELEASE_TAG`, `COMMIT_SHA` set) prints the exact request body.

### Set App Links JSON

Operator task. Since iteration 19 this lives in sovereign-config
at the `android/assetlinks-json` leaf, not in OpenBao — render it and write it to
both env subtrees:

```bash
# Dev — fingerprint of the committed keystore android/app/debug.keystore (all builds
# sign with it, so this is fixed): SHA-256
#   3A:49:7C:AE:57:AD:FF:E4:D0:C8:3B:D2:D0:98:2C:C2:98:CB:1D:B6:3F:70:68:5A:57:13:07:96:CC:9C:62:3A
./scripts/render-assetlinks-json.sh dev "$(./scripts/android-dev-debug-fingerprint.sh)" \
  | sovereign-config put /v-note/dev/server/android/assetlinks-json

# Prod — release keystore SHA-256 (keytool -list -v …), when prod Android ships:
./scripts/render-assetlinks-json.sh prod 'AA:BB:CC:...' \
  | sovereign-config put /v-note/prod/server/android/assetlinks-json
```

The value is non-secret (it is served publicly at `/.well-known/assetlinks.json`),
so it is a plain `put`, not `secret put`. The server validates it parses as JSON at
startup and refuses to start otherwise.

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
| **`v-note-android`** | `Dockerfile.android` (`--target apk`) | `registry.desync.link/v-note-android:{release}` |
| **`v-note-android-instrumented`** | `Dockerfile.android` (`--target instrumented`) | `v-note-android-instrumented:{sha}-api{29\|36}` |
| **`v-note-e2e-playwright`** | `e2e/docker-compose.test.yml` | `v-note-e2e-playwright:{release}` |

**Each Dockerfile owns its own labels.** Static values are `LABEL` instructions in
the Dockerfile; only the three per-build values arrive as build args, so every
build path — CI, `just run-compose`, a bare `docker build` — emits the same label
set:

| Label | Source |
| --- | --- |
| Static (title, description, licenses, url, source, authors, vendor, documentation) | `LABEL` in the Dockerfile |
| `org.opencontainers.image.base.name` / `.base.digest` | `LABEL` in the Dockerfile, fed from the same `BASE_IMAGE_NAME` / `BASE_IMAGE_DIGEST` args as its `FROM` (`BUILD_BOX_IMAGE_NAME` / `BUILD_BOX_IMAGE_DIGEST` for the instrumented image), so the labels cannot describe a different base than the one built on. For the android-build-box image the build-box args are also held against `scripts/android-build-box-image.ref` by `scripts/check-android-build-box-image.sh` — a file that spells the pin out must spell out the *current* one, whether as a whole `name@sha256:…` or as the two halves |
| `org.opencontainers.image.version` | `--build-arg OCI_IMAGE_VERSION` (`.release-tag` in CI, `git describe` locally) |
| `org.opencontainers.image.revision` | `--build-arg OCI_IMAGE_REVISION` (`CI_COMMIT_SHA` / `git rev-parse HEAD`) |
| `org.opencontainers.image.created` | `--build-arg OCI_IMAGE_CREATED` — **`git log -1 --format=%cI`, not the wall clock**, so rebuilding a commit reproduces the same label (and the same image config blob) |

The Playwright and legacy-migration fixture images are the exception: they are
defined as `dockerfile_inline` in `e2e/docker-compose.test.yml` with compose
`build.labels`, because they have no Dockerfile and no second build path.

**`scripts/check-image-metadata.sh`** runs against the **web**, **android** and
**instrumented** images in their build steps, and against the Playwright image in
`e2e-web` — it fails if any required label is missing or if version/revision do
not match what the step passed in. It previously ran only against the Playwright
image, which is why the web image shipped for several releases with no `authors`
or `vendor` label despite both being required.

**Compose and image builds.** `deploy/docker-compose.yml` has **no `build:`
block** and must not gain one. Compose may build images that exist *only* for
that stack (the e2e Playwright and fixture images), but never the production
application image: that has a canonical CI build, and a second definition means a
second, drifting label set and a `docker compose up --build` on mini that would
deploy host source instead of the tested image.

**Build context:** each image uses a Dockerfile-paired ignore file (BuildKit convention) so `COPY . .` cache is not busted by unrelated tree changes:

| Image | Ignore file |
| --- | --- |
| Web | `Dockerfile.web.dockerignore` |
| Android (apk + instrumented) | `Dockerfile.android.dockerignore` |
| Rust CI gates (no image) | `Dockerfile.rust-ci.dockerignore` — keeps `contracts/` and `crates/server/tests/`, which the web image excludes |
| Playwright e2e | `e2e/.dockerignore` (compose `context: e2e/`) |

Local check after build — the same three build args CI passes. Example (web):

```bash
SHA=$(git rev-parse HEAD)
CREATED="$(git log -1 --format=%cI)"

docker build -f Dockerfile.web \
  --secret id=github_token,env=GITHUB_TOKEN \
  --build-arg OCI_IMAGE_VERSION=0.3.0-local \
  --build-arg OCI_IMAGE_REVISION="$SHA" \
  --build-arg OCI_IMAGE_CREATED="$CREATED" \
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
