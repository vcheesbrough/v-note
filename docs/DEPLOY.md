# Deploy — v-note

**Status:** Authentik blueprint + OIDC env wiring land in **#146**; live deploy smoke in **#152**.

**Spec:** [`PLAN.md`](PLAN.md) · **Local dev:** [`DEV.md`](DEV.md) · **Reference:** [bored `.woodpecker/build.yml`](https://github.com/vcheesbrough/bored/blob/main/.woodpecker/build.yml)

---

## Environments

| Env | URL | Compose project (example) | DB volume (example) | OIDC scope |
| --- | --- | --- | --- | --- |
| **dev** | `https://v-notes-dev.desync.link` | `v-note-dev` | `v-note-dev-db` | `v-note:dev:access` |

**Dev is the only environment.** A production environment is specified in [`PLAN.md`](PLAN.md) but not built; **#392** removed the unexercised prod configuration and **#388** creates it for the MVP release.

**Shared:** Traefik on **mini**, Authentik at **`https://auth.desync.link`**, registry **`registry.desync.link`**.

---

## Woodpecker deploy pipeline

**Every push deploys dev, on any branch** — dev is the pre-merge environment, so a branch is deployed there to be tested before it merges, and the last push wins. (#274 restricted this to `master`; the per-environment blueprint split plus `smoke-oidc-login-auto-dev` replaced that guard — see the header comment in [`.woodpecker/deploy.yml`](../.woodpecker/deploy.yml).) Manual deployment with **`CI_PIPELINE_DEPLOY_TARGET=dev`** remains available (bored-aligned steps in `.woodpecker/deploy.yml`):

1. **validate-deployment** — manual deployment target must be `dev`, the only environment that exists; the error names **#388**
2. **compute-version** — semver from workspace + tag count (`0.N.P` pre-MVP; **`1.0.0`** after MVP **#151**)
3. **apply-authentik-blueprint-dev** — the target's own `authentik/blueprint-<env>.yaml` to **`auth.desync.link`** before roll-out (split per environment in #274 — one file, one `instance_name` — so no environment's deploy can reach another's provider)
4. **deploy** — `sovereign-config render /v-note/devops/dev/compose -- ./scripts/deploy-v-note.sh` supplies the deploy's configuration from the store (see [Secrets](#secrets)). `deploy-v-note.sh` pulls the tested image tag and runs `docker compose` on mini (docker socket), then **gates on health**: it polls the container's own healthcheck status (`HEALTHCHECK` in [`Dockerfile.web`](../Dockerfile.web), which curls `https://127.0.0.1:443/health`) and fails the deploy if it never reports healthy. `docker compose up -d` alone only proves the container was *created* — a crash-looping container would otherwise report a green deploy. Gating on the container's own status rather than a separate probe means the deploy passes on exactly the condition `docker ps` reports, and both failure modes are *decided* rather than waited out: a process that dies on bad config is caught by its **run state** (`exited` / `restarting`) in seconds — it never reports unhealthy at all, which is precisely why the old probe burned the full timeout on every crash loop — and `unhealthy` is **terminal**, because docker has already applied the configured retries. The 120s deadline now only covers an app that stays up and never finishes starting. The failure dump includes `.State.Health.Log`, i.e. the last five probe attempts with curl's own error text. **Rolling back to an image built before iteration 23** has no healthcheck to gate on; the script says so explicitly rather than polling until the deadline.
5. **tag-release** — after a successful deploy, push the git tag matching `.release-tag` so the next deployment advances the patch digit
6. **publish-grafana-dashboard** — **every push to `master`**, after `auto-deploy-dev` and alongside `tag-release-auto-dev` (it does not gate it): publishes `deploy/grafana/v-note-overview.json` to Grafana — see [Grafana dashboard](#grafana-dashboard)

Push auto-dev deploy uses the same script and literally the same environment block as manual `deploy-dev` (a YAML anchor, so they cannot drift), but it is gated by the successful push path. The gate is the **workflow-level** `depends_on` of `deploy.yml`: the `checks` workflow (`lint`, `rust-test`, `deploy-script-validation`, `grafana-dashboard-validation`, `android-build-box-pin`), the `web` workflow (`build-web`, `e2e-web`) and the `android` workflow (`build-android` and both instrumented lanes, `android-instrumented-api-29` / `-36`) must all succeed before `deploy.yml` starts at all. The dependencies are marked `optional` only so that a manual deployment — which runs none of those workflows — is not blocked; on a push all three are present and enforced.

Because workflows share nothing, `deploy.yml` computes the release tag again. The first push step, **verify-release-images**, pulls `v-note:{release}` and `v-note-android:{release}` and fails unless both carry this commit's `org.opencontainers.image.revision` — so a tag pushed by another pipeline in between can never roll out someone else's images.

### The deploy script takes no arguments

`scripts/deploy-v-note.sh` has **one entry point and no modes**. It does not know
which environments exist: every environment-specific value is a parameter set by
the calling step in [`.woodpecker/deploy.yml`](../.woodpecker/deploy.yml), where
the dev block is defined once and reused by `auto-deploy-dev` via a YAML anchor.
Adding an environment means adding a step, not editing the script.

The parameters reach it from two places, and the script cannot tell them apart —
which is the point. **Step** values are written literally in `deploy.yml`;
**rendered** values come out of `/v-note/devops/<env>/compose` via
`sovereign-config render` (#391). A missing parameter fails the same way
whichever side it should have come from.

| Parameter | From | Purpose |
| --- | --- | --- |
| `REGISTRY_USER` / `REGISTRY_PASSWORD` | step | `registry.desync.link` credentials |
| `POSTGRES_PASSWORD` | rendered | consumed directly by the `postgres` service |
| `SOVEREIGN_CONFIG_ACCESS_URL_FILE` | rendered | unlocks the app's sovereign-config subtree; **which URL is injected selects the environment** |
| `V_NOTE_METRICS_ADDR` | rendered | `host:port` or `disabled` — see [the alias note](#aliases-not-copies) |
| `APP_ENV` | rendered | the `observability.env` discovery label; the canonical environment name, never the branch |
| `COMPOSE_PROJECT_NAME` | step | compose project; read by `docker compose` itself, so the script passes no `-p` |
| `COMPOSE_FILE` | step | `:`-separated compose files; likewise no `-f` |
| `V_NOTE_IMAGE_REPOS` | step | space-separated repos to pull at the release tag (dev adds `v-note-android`) |
| `V_NOTE_HOST`, `V_NOTE_CONTAINER_NAME`, `DB_VOLUME` | step | passed straight through to compose |

All of these are required. The script explicitly checks only the four whose
absence would otherwise be **silent** — `APP_ENV` (compose falls back to `dev`,
so any other environment would mislabel itself `env=dev`), `COMPOSE_PROJECT_NAME` (compose
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

**Never commit values.** The deploy reads its configuration from sovereign-config
with the CLI; a small set of shared infrastructure credentials still arrives
through the Woodpecker broker; local compose secrets still come from OpenBao.

### The deploy renders its configuration (#391)

The deploy step wraps the deploy script in the sovereign-config CLI:

```yaml
commands:
  - sovereign-config render /v-note/devops/dev/compose -- ./scripts/deploy-v-note.sh
```

`render` reads the layer's **direct children**, puts each one in the environment
under **its leaf name exactly as stored**, and `exec`s the command. That is why
these leaves are named `POSTGRES_PASSWORD` and not `postgres-password` — a leaf
with a `-` or a leading digit is refused. It **fails closed**: an unreadable
layer, or one that contributes no values at all, means the deploy script never
runs.

#### The CLI comes from the step image

The pipeline does **not** install the CLI. Both deploy steps (`deploy-dev`,
`auto-deploy-dev`) run in
`registry.desync.link/sovereign-config-cli:2.30.2@sha256:08bf4909…1827`
(`&deploy-image` in `.woodpecker/deploy.yml`): the docker CLI image with the
sovereign-config CLI baked in, published by the operator. It carries everything
the step uses — Docker with the compose plugin, `sh`, and the busybox tools
`deploy-v-note.sh` needs — so it is a drop-in for `docker:27-cli`. Every other
step keeps `docker:27-cli`.

Until PR #55 the step installed the CLI from the server's `/dist` with a pinned
version and digest (`scripts/install-sovereign-config-cli.sh`). That was removed
when the server retired the pinned 2.26.2 installer: the download 404'd and every
deploy failed, on every branch and on master. A host bind mount of an
operator-installed binary briefly replaced it and was then dropped for this
image, because a host file drifts from the server silently while a pin changes
only by commit.

**The CLI must match the running server**, so bump the image's tag and digest
together, as a commit, when the server is upgraded (verified at pin time: 2.30.2
against 2.30.2). The other client pinned to the
server is `sovereign-config-provider` in `crates/server/Cargo.toml` (see
[Runtime config](#runtime-config-sovereign-config)); check the live version
(sovereign-config MCP `status`) before relocking it.

| `/v-note/devops/dev/compose/…` | Kind | Notes |
| --- | --- | --- |
| `POSTGRES_PASSWORD` | secret | **alias** of `/v-note/dev/server/database/password` |
| `V_NOTE_METRICS_ADDR` | plain | **alias** of `/v-note/dev/server/observability/metrics-addr` |
| `APP_ENV` | plain | **alias** of `/v-note/dev/server/observability/environment` |
| `SOVEREIGN_CONFIG_ACCESS_URL_FILE` | secret | the **app's** access URL for `/v-note/dev/server` |

Every one of the first three is an **alias**, not a copy — one stored value at
two canonical paths (see [the alias note](#aliases-not-copies) below), so the
deploy and the running server cannot disagree about the database password, the
metrics listener or the environment label.

One `/v-note/devops/<env>/compose` layer per deployed environment; dev is the
only one, and **#388** adds prod's.

> **Two URLs, near-identical names.** `SOVEREIGN_CONFIG_URL` is the **CLI's**
> credential — it unlocks `/v-note/devops`, and `render` strips it from the child
> process's environment. `SOVEREIGN_CONFIG_ACCESS_URL_FILE` is the **app's**
> credential — it unlocks `/v-note/dev/server`, and it is one of the rendered
> values, passed through to compose. They are rotated separately; see
> [Creating / rotating an access URL](#creating--rotating-an-access-url-operator).

### What is left on the Woodpecker broker

Mini points `WOODPECKER_SECRET_EXTENSION_ENDPOINT` at the
`sovereign-config-woodpecker-broker`, whose layers are
`/woodpecker/shared,/woodpecker/repos/{repo.owner}/{repo.name}` — so a
`from_secret: <name>` in any [`.woodpecker/`](../.woodpecker/) workflow resolves
at `/woodpecker/shared/<name>` or `/woodpecker/repos/vcheesbrough/v-note/<name>`.

v-note's **own** layer now holds exactly one leaf:

| Repo secret key | Used for |
| --- | --- |
| `v_note_devops_sovereign_access_url` | `SOVEREIGN_CONFIG_URL` — read-only connection **`v-note-devops`** rooted at `/v-note/devops`, the credential `render` uses above |

The rest are **shared** infrastructure credentials on `/woodpecker/shared`, used
by bored and sovereign-config too, and **not v-note's to move or remove**:
`zot_ci_user` / `zot_ci_password` (registry), `github_token` (version compute and
tag push), `authentik_api_token` (blueprint apply), `grafana_api_token` (dashboard
publish). The build and verify steps need them through the broker regardless, so
aliasing them into `/v-note/devops` would buy nothing and widen what the devops
URL can read.

Android signing (dev) needs no secret at all: the committed **non-secret** debug
keystore `android/app/debug.keystore` is shared by every build, which is what
makes the certificate and App Links fingerprint stable. Android **release**
signing needs a secret keystore held **outside the repo**; that is tracked in
**#178** and blocks #388.

Rotate a broker leaf by rewriting it in sovereign-config (`put_secret`, or
`sovereign-config set --secret <path>` with the value on stdin) at
`/woodpecker/repos/vcheesbrough/v-note/<name>`. CI injects it via Woodpecker —
**no `.env` on the host**.

> **Log masking changes with this move.** Woodpecker masks `from_secret` values in
> step logs. `POSTGRES_PASSWORD` and the app's access URL are no longer
> `from_secret`, so they are **not masked**: `docker compose config` must keep its
> `--quiet`, and `deploy-v-note.sh` must never gain `set -x`.

### The OIDC client secret is gone (#274, retired in #394)

The unified client is **public + PKCE**, so no client secret is used anywhere.
#274 removed the *references*; **#394 removed most of the *values***:

| Was | Where | Status |
| --- | --- | --- |
| The dev OIDC client-secret broker leaf | sovereign-config `/woodpecker/repos/vcheesbrough/v-note/` (the Woodpecker broker layer) | deleted |
| `oidc/client-secret` | sovereign-config `/v-note/dev/server/` | deleted |
| `oidc/android/client-id`, `oidc/android/issuer-url` | sovereign-config `/v-note/dev/server/` — named the `v-note-android-dev` client that #274 retired | deleted |
| `OIDC_CLIENT_SECRET` | OpenBao `secret/v-note-stack/env` | **still present** — deletion needs a `BAO_TOKEN`; tracked on **#394** |

The OpenBao copy is inert (`scripts/fetch-compose-env.sh` requires only
`POSTGRES_PASSWORD`, so nothing reads it) but it is a **live stored credential**.
Do not treat this section as saying every copy is gone until that row says
`deleted`.

`OidcConfig` has no `client_secret` or `android` field, and
`scripts/fetch-compose-env.sh` requires only `POSTGRES_PASSWORD`, so nothing
reads any of them. `crates/server/src/config/tests.rs` keeps
`oidc_ignores_retired_android_subtree` and `oidc_ignores_retired_client_secret`
so that a stale leaf reappearing in a subtree still cannot fail a load.

> **Rollback depth is bounded by #274, not by these values.** A pre-#274 image
> refuses to start without a non-empty `oidc/client-secret` — but it also
> authenticates as a *confidential* client, and the live provider has been
> `client_type: public` with no secret since #274. Keeping the leaf would have
> let such an image boot without letting anyone log in, so it protected nothing:
> the rollback it appeared to guard had already been broken by the Authentik-side
> change. That is why #394 deleted the values rather than waiting for the
> registry to age out images that cannot work anyway.

### Grant types are not optional in a blueprint (#372)

A provider created by a blueprint gets **no grant types at all** unless the
blueprint names them. Authentik does not fall back to a sensible default and
does not report the omission: the provider applies cleanly, looks right in the
admin UI, and then rejects **every** authorize request with

```
error=invalid_request&error_description=The request is otherwise malformed
```

which the server renders as `authentication denied: invalid_request` — a blank
page with one line of text. #274 created the provider this way, and dev login was
down until #372. So the blueprint declares:

```yaml
grant_types:
  - authorization_code
  - refresh_token
```

`refresh_token` is required because the Android client requests `offline_access`
and refreshes. Nothing else belongs there: the provider is a **public** client
with no secret, so `password` and `client_credentials` must stay off, and
`implicit`/`hybrid` would hand out a token without the PKCE-bound code exchange.
`scripts/test-authentik-blueprints.sh` enforces exactly that list, and
`scripts/smoke-oidc-login.sh` re-checks it against the live IdP after each
deploy — see [DEV.md](DEV.md) for running both locally.

**This is a blueprint-authoring trap, not a one-off:** any new Authentik
provider added here needs its `grant_types` spelled out.

**Recovering an environment after the fix lands.** Correcting the blueprint does
not correct the live provider — the blueprint has to be *applied*:

| Environment | Applied by | When |
| --- | --- | --- |
| dev | `apply-authentik-blueprint-auto-dev` | automatically, on every push, from any branch |

So pushing the fix restores dev on its own, and `smoke-oidc-login-auto-dev`
verifies it on the same pipeline. An environment applied only by a *manual*
deployment (as #388's will be) stays broken until someone runs one. Confirm an
environment by hand with:

```bash
V_NOTE_HOST=v-notes-dev.desync.link ./scripts/smoke-oidc-login.sh
```

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

> **Blast radius.** The blueprint is split per environment
> (`authentik/blueprint-<env>.yaml`, one `instance_name` per file) and each
> pipeline step applies only its own target's file, so no environment's deploy can
> reach another's provider. That split is the guard, and it exists because a single
> combined file, applied from a feature-branch push, deleted the live providers for
> **every** environment during #274.
>
> The push-path apply runs on **every branch**, because dev is the pre-merge
> environment: a branch that changes the provider has to be able to apply it, or
> auth work cannot be tested before it merges. #274 additionally restricted this
> path to `master`, which removed branch deploys entirely; that restriction was
> lifted in iteration 47. A branch can still break **dev** login for everyone
> until it is fixed, reverted, or `master` is re-pushed — accepted deliberately,
> and `smoke-oidc-login-auto-dev` now fails the branch that does it.
>
> **Operator step, once — tracked as [#367](https://bored.desync.link/boards/v-notes?card=367), sequenced after #274 deploys.**
> The pre-split blueprint instance is still registered in Authentik under
> `instance_name: v-note`, holding the *combined* pre-split content. Authentik
> re-applies registered instances on its own schedule, so **until it is deleted the
> split above is enforced in the pipeline but not in the live system** — the
> combined content keeps being reasserted. Delete it only after a deploy has
> created `v-note-dev`, then confirm the authorize endpoint still returns 302.
>
> **#392 leftover — live objects with no file describing them.** That combined
> content also declared the prod objects: the `v-note-prod` OAuth2 provider and
> `v-note (prod)` application, the `v-note-prod-users` group and its bindings,
> and the `v-note:prod:access` scope mapping. `authentik/blueprint-prod.yaml` is
> gone from the repo, so **nothing here describes them any more, but they are
> still live and still reasserted** until the `v-note` instance is deleted.
> Deleting that instance is what removes them, and it must be deleted *before*
> the objects themselves or a re-apply resurrects them. Tracked as Part B of
> [#392](https://bored.desync.link/boards/v-notes?card=392); #388 recreates them
> from a fresh `blueprint-prod.yaml`.

<a id="aliases-not-copies"></a>

**The `/v-note/devops/dev/compose` leaves are aliases, not copies.** `AddValuePath`
exposes one stored value at several canonical paths, so the deploy and the app
read the *same* leaf. The app resolves `database/password`,
`observability/metrics-addr` and `observability/environment` through its own
sovereign-config client; `render` hands the deploy step the same three values
under the names compose and `deploy-v-note.sh` expect. `deploy-v-note.sh` uses
`V_NOTE_METRICS_ADDR` to derive the Alloy `observability.metrics.port` /
`.scrape` container labels, which docker writes at container-create time and
nothing inside the container can influence. Create them with:

```bash
sovereign-config alias add /v-note/dev/server/database/password \
  /v-note/devops/dev/compose/POSTGRES_PASSWORD
sovereign-config alias add /v-note/dev/server/observability/metrics-addr \
  /v-note/devops/dev/compose/V_NOTE_METRICS_ADDR
sovereign-config alias add /v-note/dev/server/observability/environment \
  /v-note/devops/dev/compose/APP_ENV
```

One set per deployed environment; dev is the only one today.

The alias direction is deliberate: each value's canonical home is the **app**
subtree, and `/v-note/devops` is the view the deploy pipeline is allowed to read.
Aliasing widens read access — anything holding the devops URL can read these —
which is why only values the deploy genuinely needs are exposed there, and why
`APP_ENV` (a hand-kept copy until #391) was folded in rather than left to drift.

App Links JSON is **not** in this list: since iteration 19 it lives in sovereign-config
at `android/assetlinks-json`. The former `v_note_<env>_assetlinks_json` keys have
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
under `/v-note/<env>/server` — today only `/v-note/dev/server` — not in compose
env vars. The server loads four independent groups — `database`, `oidc`,
`observability`, `android` — and **refuses to start (non-zero exit, redacted
error) if any value is missing or invalid**.

The deploy step gets the per-env access URL as the
`SOVEREIGN_CONFIG_ACCESS_URL_FILE` env var — rendered from
`/v-note/devops/<env>/compose/SOVEREIGN_CONFIG_ACCESS_URL_FILE`, not brokered in
(#391); compose sources a docker secret of the same name straight from it and
mounts it at `/run/secrets/SOVEREIGN_CONFIG_ACCESS_URL_FILE`, which the container's
`SOVEREIGN_CONFIG_ACCESS_URL_FILE` points at. **The URL is itself a secret and
selects the environment** — which environment's config the server reads is decided
by which URL is injected, not by a config flag.

### Creating / rotating an access URL (operator)

There are **two** URLs, and they are rotated independently. Both grant read
access to a whole subtree, secret leaves included — treat each like a password,
and never let one reach a command argument, a tool call or shell history.

**The app's URL** — connection `v-note-dev-server`, root `/v-note/dev/server`:

1. Create or rotate the managed connection (web UI, or `rotate_connection`). The
   URL is displayed **once**.
2. Store it in the deploy's own layer, with the URL on **stdin** so it never
   reaches argv or shell history:
   ```bash
   sovereign-config set --secret \
     /v-note/devops/dev/compose/SOVEREIGN_CONFIG_ACCESS_URL_FILE
   ```
3. Redeploy — the next `render` picks it up. No app change needed.

**The CLI's URL** — connection `v-note-devops`, root `/v-note/devops`, `read`
only. This is the one Woodpecker secret v-note still owns:

1. Create or rotate the connection in the web UI.
2. Store it at `/woodpecker/repos/vcheesbrough/v-note/v_note_devops_sovereign_access_url`
   — paste into the UI, or `sovereign-config set --secret <that path>` with the
   URL on stdin.
3. Redeploy.

Note the ordering trap: the app's URL lives *inside* the subtree the devops URL
unlocks, so the devops URL must be valid before a deploy can read anything at
all. `render` fails closed, so a bad devops URL stops the deploy rather than
half-configuring it.

The secret leaf (`database/password`) is stored with `put_secret` and revealed to
the app at load. `POSTGRES_PASSWORD` is **the same stored value**, aliased into
the deploy's layer — not a second copy, and no longer in OpenBao for deployed
environments. Local compose still keeps its own `POSTGRES_PASSWORD` in OpenBao,
because that is a different database.

The provider is pinned to tag **2.30.2**, the sovereign-config server version
deployed when it was last bumped. Since 2.25 the provider **negotiates** a protocol
version with the server on connect and fails closed only when they share none, so
a server upgrade no longer requires rebuilding v-note: a build keeps working until
the server *retires* every protocol version it speaks — an announced, observable
event. Bumping the pin is still worthwhile to pick up client fixes; when you do,
read the `## Upgrade` section of sovereign-config's `README.md` at the new tag for
source-breaking changes to the client crates, and compare against the live version
(sovereign-config MCP `status`, or the unauthenticated gRPC `System.GetVersion`).

### Compose env vars that remain (per deployment)

| Variable (example) | Purpose |
| --- | --- |
| `V_NOTE_HOST` | `v-notes-dev.desync.link` — **compose-level only**, for the Traefik router rules; the container is not given it |
| `V_NOTE_CONTAINER_NAME` | `v-note-dev` |
| `DB_VOLUME` | `v-note-dev-db` |
| `APP_ENV` | compose-level only — the `observability.env` discovery label |
| `APP_VERSION` | compose-level only — the `observability.release` discovery label (the server's own `/api/meta` version is baked in at build via `V_NOTE_RELEASE`, not read here) |
| `SOVEREIGN_CONFIG_ACCESS_URL_FILE` | in-container path to the access-URL secret; blank disables the sovereign layer |
| `V_NOTE_METRICS_ADDR` | compose-level only — `host:port` or `disabled`, rendered from `/v-note/devops/<env>/compose/V_NOTE_METRICS_ADDR`, an alias of `observability/metrics-addr`. `deploy-v-note.sh` derives `observability.metrics.port` and `observability.metrics.scrape` from it, so the listener and the thing scraping it read one value. **Required** — a missing value fails the deploy rather than defaulting |
| `COMPOSE_PROFILES` | `sqltool` enables the [SQL console](#sql-console-dbconsole); omit it and no console container is created. Read by `docker compose` itself |
| `PGWEB_DB_PASSWORD` | the `v_note_pgweb` role's password. **Required only when the `sqltool` profile is active**, and there is deliberately no fallback — see the console section |
| `PGWEB_AUTH_USER` / `PGWEB_AUTH_PASS` | the console's basic-auth backstop. `deploy-v-note.sh` derives `PGWEB_AUTH_B64` from them for the Traefik header that satisfies it, so the two cannot drift apart |

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
above. Example shape: `deploy/assetlinks.dev.json` and
`deploy/assetlinks.example.json` (documentation only — do not commit real
fingerprints).

## Observability

v-note integrates with the mini-config monitoring stack on `proxy-backend`:

- **Metrics:** the app serves Prometheus text on internal port `9090` at `/metrics`. Alloy discovers it through Docker labels on the `v-note` service: `observability.metrics.scrape=true`, `observability.metrics.port=9090`, `observability.metrics.path=/metrics`, `observability.metrics.scheme=http`, `observability.service=v-note`, `observability.env`, `observability.release`, and `observability.protocol`.
- **Traces:** the `observability` config group sets `otlp-endpoint=http://monitor-alloy:4317`, `otlp-protocol=grpc`, and `service-name=v-note` in each deployed environment's subtree; `observability/environment` supplies the OTEL `deployment.environment` attribute (`dev` / `production`). Every `http.request` span adopts the W3C `traceparent` Traefik forwards, so a request's trace starts at Traefik's edge span and drills down into v-note. WebSocket connection spans nest under their upgrade request; each inbound page message is its own trace, linked to its connection span. A broadcast carries its publisher's trace context, so each socket's send of a fanned-out message is a `realtime.fanout.deliver` span (`channel`, `message_type`, `bytes`, plus `session_id` on the page channel) inside the publisher's trace — a `commit-batch` trace shows the delivery to every sibling session — and linked to the receiving connection. Every Postgres round trip — each query, and each transaction's `BEGIN`/`COMMIT` — is a `db.query` span carrying `db.operation` and `db.query_name` (the call site, never SQL text or values), plus the size of the result: `db.response.returned_rows`, `db.response.bytes` and `db.response.max_row_bytes` (Postgres wire bytes of the returned column values, in total and for the widest row), and `db.response.affected_rows` for writes. Every span also carries the OpenTelemetry `code.file.path` / `code.module.name` / `code.line.number` the tracing layer derives from where the span was opened; `db_query_span!` is a macro so that those name the query's own call site rather than `observability.rs` (#343). Thumbnail jobs run detached, so each is its own `thumbnail.generate` trace (with `db.query` and `thumbnail.render` children) linked to the request that queued it.
- **Client telemetry (#354):** the SPA's traces and logs arrive at `POST /otlp/{spa|android}/v1/{traces|logs}` on the app, are authenticated there (cookie or bearer, plus `required-scope`; body capped at 1 MiB → 413; sidecar down → prompt 502), and are proxied to the **`${V_NOTE_CONTAINER_NAME}-alloy`** sidecar running the committed `deploy/alloy/client-telemetry.alloy`. The sidecar has **no Traefik route** — the app is its only caller — and exports to `monitor-tempo:4317` (traces) and `http://monitor-loki:3100/otlp` (logs), overridable with `CLIENT_TELEMETRY_TEMPO_ENDPOINT` / `CLIENT_TELEMETRY_LOKI_ENDPOINT`. Its config reaches the container as a compose `configs:` entry that `deploy-v-note.sh` fills from the file (a bind mount would resolve on the host, where the CI workspace does not exist). It is **not** in the deploy health gate: telemetry must never fail a product deploy. Ingest is **off** until the environment's sovereign-config subtree sets `client-telemetry/enabled = "true"`, `client-telemetry/spa-endpoint = "http://<container>-alloy:4318"` and `client-telemetry/android-endpoint = "http://<container>-alloy:4319"` (bare origins; a path is rejected at startup). Measured on #354: the sidecar idles at ~60 MiB RSS and ~0.1% CPU, with `mem_limit: 192m` above its 64 MiB memory limiter. Charted on the dashboard's **Client telemetry** row; no alert, since losing telemetry is not user-facing.
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

- **One dashboard per environment:** an `env` variable — a **constant** pinned to `dev`, `hide: 2` — filters every query. It was a `label_values(v_note_realtime_active_connections, env)` query until **#392**: that would have silently widened to any new `env` series the moment one appeared, so it is pinned while dev is the only deployment. **#388** copies this dashboard under a new uid and changes the constant; every panel keeps `env="$env"`, so nothing else moves. Prometheus is referenced by uid `PBFA97CFB590B2093`, Loki by `P8E80F9AEF21F6940`. A dashboard link opens a Tempo TraceQL search for the selected env.
- **Published by CI:** the `publish-grafana-dashboard` step in `.woodpecker/deploy.yml` runs [`scripts/publish-grafana-dashboard.sh`](../scripts/publish-grafana-dashboard.sh) on **every push, on any branch**, after `auto-deploy-dev`, so the dashboard always matches what is deployed to dev — a branch's panels go live with the metrics they chart and can be checked before merge. It posts `{dashboard (id: null), folderUid, overwrite: true, message: "v-note <branch> <release> <sha>"}` to `/api/dashboards/db` with the shared `grafana_api_token` (`woodpecker-ci` service account, Edit on the Applications folder). So every entry in the dashboard's version history names its branch and commit. A non-2xx fails the step and prints Grafana's response body; the token is never printed. The last push wins, as it does for dev itself.
- **UI edits are overwritten** on the next push to `master` that deploys dev. To change the dashboard, edit it in Grafana (a scratch copy is fine), export the JSON into the repo file, and keep `uid: v-note-overview` with no numeric `id`.
- **Offline validation:** [`scripts/test-grafana-dashboard.sh`](../scripts/test-grafana-dashboard.sh), run in the `checks` step `grafana-dashboard-validation`, checks that the JSON parses, keeps its uid, has no committed id, filters every query by `env`, references no unbounded id, and charts only metrics `observability.rs` registers. It also checks the publish script's `--dry-run` payload, its input guards, and its live path against a stub `curl` (2xx passes; non-2xx and transport failures fail without leaking the token).
- **Pre-merge check:** since #274 a branch push publishes **nothing** — the dashboard follows `master` only, in step with dev. Validate a dashboard change offline with the `--dry-run` below and the `checks` step above, then check the panels under Applications / v-note after the merge lands on dev, and record that in the PR. `./scripts/publish-grafana-dashboard.sh --dry-run deploy/grafana/v-note-overview.json` (with `GRAFANA_FOLDER_UID`, `RELEASE_TAG`, `COMMIT_SHA` set) prints the exact request body.

### Set App Links JSON

Operator task. Since iteration 19 this lives in sovereign-config
at the `android/assetlinks-json` leaf, not in OpenBao — render it and write it to
each deployed environment's subtree:

```bash
# Dev — fingerprint of the committed keystore android/app/debug.keystore (all builds
# sign with it, so this is fixed): SHA-256
#   3A:49:7C:AE:57:AD:FF:E4:D0:C8:3B:D2:D0:98:2C:C2:98:CB:1D:B6:3F:70:68:5A:57:13:07:96:CC:9C:62:3A
./scripts/render-assetlinks-json.sh dev "$(./scripts/android-dev-debug-fingerprint.sh)" \
  | sovereign-config set /v-note/dev/server/android/assetlinks-json
```

A second environment (#388) adds an arm to `render-assetlinks-json.sh` and a leaf
under its own subtree; it needs the release keystore from **#178** first, because
the fingerprint here must match the certificate the APK is signed with.

The value is non-secret (it is served publicly at `/.well-known/assetlinks.json`),
so it is a plain `set`, not `set --secret`. The server validates it parses as JSON at
startup and refuses to start otherwise.

Obtain SHA-256: `./scripts/android-dev-debug-fingerprint.sh` (local debug keystore), `--docker` only for the CI image keystore, or `keytool -list -v` on a release keystore (**#178**).

---

## Image tags

| Phase | Dev |
| --- | --- |
| Pre-MVP | `0.N.P-<sha>` |
| MVP **1.0.0** | `1.0.0-<sha>` + git tag **`v1.0.0`** |
| Post-MVP | `1.N.P-<sha>` |

Dev is the only environment that is deployed today; **#388** defines the release
environment's tags when it creates it.

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

**Dev only** — dev is the only deploy target (#392), so this is the only
environment there is to roll back. Redeploy a **previous image tag** via
Woodpecker manual deploy (`CI_PIPELINE_DEPLOY_TARGET=dev`) with pinned version env
(detail in **#152** runbook). **Also reinstall the matching Android APK.**

**Floor: `0.45.0` — do not roll back past it.** Pre-#274 images do not work
against the current Authentik provider and config (#394); treat them as
unusable rather than as rollback depth.

`registry.desync.link` still carries tags back to `0.28.1` for both `v-note` and
`v-note-android`, so the floor is a **convention, not an enforced limit**.
Pruning those tags is not done: the `ci` registry account has push/pull but no
delete permission (every `DELETE /v2/<repo>/manifests/<digest>` returns `403`),
so it needs a Zot credential with delete rights or a `storage.retention` rule in
`mini-config`.

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

## SQL console (`/dbconsole`)

A read-only Postgres console (`pgweb`) served from the **same host as the app**,
at `https://v-notes-dev.desync.link/dbconsole/`. It exists so a number on a
dashboard or in a trace can be checked against the rows that produced it without
shelling into the host.

### Who can reach it

Two independent gates, both of which must pass:

1. **Authentik forward auth** — the identity gate. Access is restricted to the
   **`v-note-dev-admins`** group, via the console's own Authentik application
   (`v-note-sql-console-dev`). This is a *different* group from
   `v-note-dev-users`: "can use v-note" and "can read every row of everyone's
   notes" are deliberately not the same permission.
2. **pgweb's own basic auth** — the backstop. Traefik injects the credential
   *after* Authentik approves, so nobody is ever prompted for it. It exists so
   the console stays closed if the outpost is down, the provider is
   misconfigured, or the profile is enabled somewhere no auth is attached.

**To grant someone access:** add them to `v-note-dev-admins` in the Authentik UI.
That is the only manual step — the group itself is created by the blueprint.

### Enabling and disabling

The toggle is the `sqltool` **compose profile**, set in `.woodpecker/deploy.yml`:

```yaml
COMPOSE_PROFILES: sqltool
```

Remove it and redeploy to turn the console off. `deploy-v-note.sh` then runs
`docker compose --profile sqltool rm -sf sqltool` explicitly — note that
**`docker compose up -d --remove-orphans` does NOT stop it**, because a profiled
service is still *defined* in the file and compose does not treat it as an
orphan. Without that explicit teardown the console would keep serving after
being switched off.

### Why three Traefik routers

The least obvious part of `deploy/docker-compose.yml`, and the thing to read
before changing any of it.

Authentik builds its forward-auth callback from the provider's **hostname** and
**discards any path** — a provider whose external host is
`https://<host>/dbconsole` still sends users back to
`https://<host>/outpost.goauthentik.io/callback`. That path does not start with
`/dbconsole`, so without a router for it the callback matches the **app's**
router, which has no Authentik middleware; the SPA's catch-all serves
`index.html`, the session cookie is never set, and login loops forever.

Every other service protected this way on this LAN is a bare `Host(…)` router,
which catches the callback by accident. A path-scoped console has to catch it on
purpose. Hence:

| Router | Rule | Purpose |
| --- | --- | --- |
| app | `Host(…)` | unchanged |
| `…-sqltool` | `Host(…) && PathPrefix(/dbconsole)` | the console |
| `…-sqltool-callback` | `Host(…) && PathPrefix(/outpost.goauthentik.io)` | the callback; the middleware answers it, nothing is proxied |

A **dedicated subdomain** would avoid all of this, and is the shape every other
protected service uses — it was rejected because `desync.link` has no wildcard
DNS record, so it would need a manual Route53 entry.

### The provider must be attached to the outpost

A proxy provider does nothing until an outpost serves it; the forward-auth
endpoint answers **404** for a host no attached provider claims, and
`/dbconsole` is then unreachable.

This is **not** done by the blueprint, and must never be. The embedded outpost is
**shared** — its provider list also carries the providers protecting glances,
woodpecker, uptime-kuma and the Traefik dashboard — and a blueprint writes a list
wholesale rather than appending, so an outpost entry in
`authentik/blueprint-dev.yaml` would detach all of them. That file is applied on
**every branch push**, not just master.

`scripts/attach-sqltool-outpost.sh` does an append-only read-modify-write
instead, runs on every deploy, is idempotent, and refuses to write a list that
lost an entry.

### What read-only does and does not prevent

The console connects as **`v_note_pgweb`**, a dedicated non-superuser role
created by `crates/server/migrations/20260920140000_pgweb_readonly_role.sql`.
That role, not pgweb's `--readonly` flag, is the actual boundary:

- pgweb's `--readonly` is a **keyword filter over the submitted text** plus a
  read-only transaction. Neither has anything to say about a `SELECT`.
- Pointed at the *application's* user — which the postgres image creates as a
  **superuser** — that "read-only" console returns the contents of `/etc/passwd`
  via `pg_read_file`. This was measured, not theorised.
- 🚫 So **never** give the console `POSTGRES_PASSWORD`, and never add a fallback
  like `${PGWEB_DB_PASSWORD:-$POSTGRES_PASSWORD}`. If the password is missing the
  deploy **must** fail; there is no degraded mode. `scripts/test-deploy-v-note.sh`
  asserts this.

The role also carries `default_transaction_read_only`, a 30s `statement_timeout`,
and `log_statement = 'all'` — all role-scoped, so the application is unaffected.

### Audit trail

**There is no per-user attribution.** `--log-forwarded-user` reads
`X-Forwarded-User`, and the Authentik middleware forwards `X-authentik-username`
instead; the basic-auth credential is shared. What you get:

- **what ran** — `log_statement = 'all'` on `v_note_pgweb`, in the postgres
  container log. This is the real record.
- **that access was gated** — the Traefik access log, plus the fact that
  Authentik approved it.

Do not treat this as an audit trail that names people.

### Break-glass

The console is a convenience, not the only way in. For anything it refuses —
including every write — use psql on the host directly:

```sh
docker compose exec postgres psql -U v_note -d v_note
```

### ⚠️ Editing a migration after you have pushed the branch

Not console-specific, but #299 hit it and it will happen again.

**A branch push deploys dev**, so the first push applies your new migration to
the *real* dev database and records its checksum. Editing that file afterwards —
even on an unmerged branch, even before review — makes the next deploy fail with:

```
Error: database migrations should apply:
  migration <version> was previously applied but has been modified
```

The app then crash-loops (`Restarting (1)`) and the deploy's health gate fails.
Nothing is wrong with the migration; the recorded checksum simply predates the
edit.

For a migration that has **never been on `master`**, clear the stale row and let
the corrected version re-apply, rather than adding a second migration to correct
one that never shipped:

```sh
ssh vincent@mini.home
docker exec v-note-dev-postgres-1 psql -U v_note -d v_note \
  -c "DELETE FROM _sqlx_migrations WHERE version = <version>"
```

The app is under `restart: unless-stopped`, so it re-applies and goes healthy on
its own within a few seconds — no redeploy needed. Check with
`docker ps --filter name=v-note-dev`.

Once a migration **has** shipped to `master`, this is no longer an option: write
a new migration instead.

### Rotating the credentials

Both live in sovereign-config under `/v-note/devops/dev/compose`:

| Leaf | What |
| --- | --- |
| `PGWEB_DB_PASSWORD` | the `v_note_pgweb` database password |
| `PGWEB_AUTH_USER` / `PGWEB_AUTH_PASS` | the basic-auth backstop |

Change the leaf and redeploy. `deploy-v-note.sh` re-applies the database
password with an idempotent `ALTER ROLE` on every deploy, so rotation needs no
manual visit to the database.

---

## Post-deploy smoke

- **#145:** pipeline structure only; optional manual curl
- **#152:** automated live smoke (TLS, Authentik, realtime, CSP)
- **#299:** `scripts/smoke-sql-console.sh` — the only automated assertion that
  the SQL console is not publicly readable, that its callback router exists, and
  that gating it did not break SPA login on the same host. No CI stack can make
  these claims (e2e has neither Traefik nor Authentik), so it gates the release
  tag alongside the OIDC smoke check.
