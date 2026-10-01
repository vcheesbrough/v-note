# Agent guide — v-note

This file holds the **v-note-specific** working rules. The machine-global rules
common to every repo — Kanban/bored workflow, iteration & versioning defaults,
branching & git safety, CI-after-push, PR self-review + comment loop, test
coverage, and MCP/secrets discipline — live in the shared **agent-shared
baseline**, imported on this machine via `~/.codex/AGENTS.md` and
`~/.claude/CLAUDE.md`. **Read that baseline first;** this file only records what
is specific to v-note or overrides the baseline.

If you change a v-note rule, change it here — there is no parallel copy.
Cross-repo rules change in `agent-shared`, not here.

**Product, stack, and architecture** live in [`docs/PLAN.md`](docs/PLAN.md) —
treat that document as the spec source of truth. **Repository overview** is in
[`README.md`](README.md).

---

## Repository context

| Item | Value |
| --- | --- |
| **Local path** | `/home/vincent/dev/v-note` |
| **Remote / `gh` repo** | [vcheesbrough/v-note](https://github.com/vcheesbrough/v-note) |
| **Trunk** | `master` |
| **Layout** | **Monorepo** (server, SPA, Android, deploy, schemas — when scaffolded) |
| **License** | **PolyForm Noncommercial 1.0.0** — see [`LICENSE`](LICENSE), [`LICENSE-TIER.md`](LICENSE-TIER.md) |
| **Kanban board** | **[v-notes](https://bored.desync.link/boards/v-notes)** (not the bored product board) |
| **Phase** | **Minimal bootstrap done** — planning locked; **#145** lands the monorepo scaffold + push CI; pre-MVP semver `0.N.P` |

**Stack summary:** Rust (Axum) server, PostgreSQL, Leptos/Trunk SPA, native
Kotlin (Compose) Android, Authentik OIDC. Locked decisions live in
[`docs/PLAN.md`](docs/PLAN.md).

### Versioning specifics (extends baseline §2)

- **Minor `N` = iteration number**, assigned only when work starts, **globally
  sequential** (highest existing `N` + 1 across Done + In Progress).
- **Pre-MVP** (before **`vertical-slice-mvp`** closes on trunk): `0.N.0`.
- **MVP release:** when the **MVP-completion card** (**#151** today) merges to
  trunk, set the workspace to `1.0.0` and tag `v1.0.0`.
- **Post-MVP:** `1.N.P`, `N` continuing from the last pre-MVP iteration.
- **Do not assume** a fixed backbone — new MVP iterations may be inserted before
  release; the backbone is **not** frozen at **#145–#151**. Discover new
  iterations → `create_card` in TODO and update the **Kanban card map** in the
  plan.
- On **In progress**, `update_card` to record **Branch:** `feat/iteration-N-short-slug`
  and **Version:** `0.N.0` / `1.N.0` in the card body once a workspace exists.

### Planning vs implementation (scope gate)

Cards in **TODO / backlog** are **spec only** until moved to **In progress**. Do
**not** treat runbook or design discussion as permission to edit the repo.

| User intent | Allowed edits | Not allowed |
| --- | --- | --- |
| Refine / update a **TODO** card (runbook, acceptance, order) | **bored MCP** `update_card` (and **`docs/PLAN.md`** only if the user asks for plan alignment) | Source code, CI, scripts, compose, new files, commits |
| **Start iteration** — *pick up #N*, *implement #N*, *start work*, *open PR* | Run the **`start-iteration`** skill (baseline §2), then implement per the active card | Scope beyond the card without replanning |
| Ambiguous — shaping a card vs building it | **Ask once:** *"Ticket only, or implement?"* then follow the answer | Guessing and coding "helpfully" |

**While a card is TODO:** the card body is the source of truth for acceptance,
runbook, expected paths, and out-of-scope — not the repo. **One deliverable per
request** — ticket update **or** plan update **or** implementation. **Verify
before writing** facts into a card/plan by reading **this repo's** pipeline,
plugins, and scripts; do not copy formats from **bored** or other repos unless
v-note uses the same mechanism. Use placeholders like **`{release}`** and define
them once (v-note: Woodpecker **`compute-version`** → **`.release-tag`**, plain
semver `MAJOR.MINOR.PATCH`).

**If you overstepped** (repo edited when only the card should change): revert
repo changes immediately; keep or fix the card; do not "finish" the
implementation in the same turn unless the user then asks to start work.

### bored MCP — repo note

`bored` MCP is used for **v-notes** board task tracking and shares the same
`bored-mcp` binary as the bored product repo (rebuild `cargo build -p mcp --release`
in the [bored](/home/vincent/dev/bored) repo when MCP **code** changes). v-note
has **no repo-shipped MCP launcher** during bootstrap.

---

## 1. Plan alignment

1. **Read the plan first.** Before significant work, skim [`docs/PLAN.md`](docs/PLAN.md)
   — especially **Project todos**, **Stack decisions**, and **Decisions made**.
2. **Reconcile with reality.** Compare the plan, active card, and any user
   request to the current source tree. If scope or facts drifted, `update`
   [`docs/PLAN.md`](docs/PLAN.md) and/or the card so they stay accurate. Do not
   implement against a stale spec silently.

---

## 2. CI / pipelines (repo specifics)

Run the **`ci-watch`** skill (baseline §4) once push CI exists. Skill
parameters: `OWNER=vcheesbrough`, `REPO=v-note`. Repo specifics:

- **PR review agent — disabled, do not rely on it.**
  [`.woodpecker/pr-review.yml`](.woodpecker/pr-review.yml) is gated behind
  `when: evaluate: 'false'` (since 2026-06-06, Claude account/OAuth issues), so
  **no remote review runs on any PR**. Its
  [`.woodpecker/pr-review-prompt.md`](.woodpecker/pr-review-prompt.md) is dead
  with it and must **not** be applied as a rubric. PR review here is the local
  `pr-self-review` subagent only (baseline §5, §3 below). The pipeline and its
  setup notes ([`docs/PR-AGENT.md`](docs/PR-AGENT.md)) are kept only so the
  documented re-enable path still works.
- **Push CI is four Woodpecker workflows** in [`.woodpecker/`](.woodpecker/):
  `checks` (lint, rust-test, deploy-script-validation, deploy-pipeline-validation,
  grafana-dashboard-validation, lane-key-validation, android-build-box-pin),
  `web` (build-web → e2e-web) and `android` (build-android — which also gates
  ktlint, detekt and Android Lint — plus API 29/36 instrumented) run in parallel; `verify-tag-deploy`
  (verify-release-images → tag-release) runs only when all three succeed.
  **A push never deploys dev (#462)** — it builds, tests and tags, on any branch,
  without touching shared infrastructure, so parallel iterations' pipelines do not
  queue behind or overwrite each other's dev. **Dev is deployed by a manual
  Woodpecker deployment** (target `dev`) of a commit whose push pipeline is green,
  from **any branch**: verify-release-images (refuses an untagged commit) →
  blueprint → outpost → deploy-dev → smoke-oidc-login ∥ smoke-sql-console ∥
  smoke-web-live ∥ dashboard publish. A deployment applies `blueprint-dev.yaml`
  to shared Authentik and redeploys dev, and the last deployment wins.
  `smoke-web-live-dev` (#179) signs in to dev through the **real** Authentik as
  the blueprint's `v-note-smoke-dev` user and inks a page (`e2e/live/`, its own
  Playwright config — never part of the mock-IdP suite); it fails the deployment.
  **A green push pipeline is not proof the build works on dev** — when a card's
  change needs the live check (auth, blueprint, deploy, migrations, the
  dashboard), deploy the branch and watch that deployment too. **Merges to
  `master` do not deploy either** (deliberate, #462): the live smokes run only on
  a deployment, so a merge that breaks real login stays undetected until someone
  deploys — deploy master after merging such a change. **There is no prod
  (#392); dev is the only deploy target until #388.** **`ci-watch` must follow
  every workflow of the pushed commit's pipeline to completion** — one green
  workflow while another is still running is not a result. Do not fold them back
  into one file: Woodpecker runs step `depends_on` as whole stages, so steps in one
  workflow wait on unrelated slow steps (#320). Reproduce commands per
  [`docs/DEV.md`](docs/DEV.md):
  - `just rust-ci lint` / `just rust-ci test` (`Dockerfile.rust-ci`, as the `lint` / `rust-test` steps)
  - `docker build -f Dockerfile.web -t v-note:ci-local --secret id=github_token,env=GITHUB_TOKEN .`
  - `TEST_IMAGE=v-note:ci-local docker compose -f e2e/docker-compose.test.yml -f e2e/docker-compose.android-apk.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright`
  - **All push workflows, including `e2e-web` and both Android lanes, must be green** before an iteration is done.
  - **Lane keys skip unchanged tests (#467).** `e2e-web` and each Android
    instrumented step hash exactly what their lane reads
    ([`scripts/lane-key.sh`](scripts/lane-key.sh): the files the lane's
    `.dockerignore` admits, plus declared extras such as `e2e/`) and exit green
    with a `lane-key: SKIPPING` line when that key already passed — docs-only
    pushes, the other lane's changes, and the master merge of an
    already-synced branch. A skipped step still counts as green: it names the
    commit and pipeline that passed. Images are **always** built, so release
    tags and deploys are unaffected. Markers live at `refs/ci/green/<step>/<key>`
    on GitHub. **Run a manual pipeline with `FULL_RUN=1`** to force every lane
    (flake hunting, a suspected stale key). When a lane's step starts reading a
    new path outside its build context, add it to `lane_extras` —
    `lane-key-validation` in `checks` fails until you do.
- **E2E policy (locked):** every **user-facing feature** in an iteration card
  must have **automated e2e tests in CI** before that card merges (see
  [`docs/PLAN.md`](docs/PLAN.md) **E2E testing**). Contract/unit tests
  supplement, they do not replace e2e. This **strengthens** baseline §6 — no
  manual-only or runbook-only acceptance for product behaviour.
- **Observability policy:** for every implementation ticket, consciously decide
  whether the change needs updates to **metrics, logs, spans/traces,
  trace-log correlation, labels, dashboards, alerts, or runbook/docs**. "No
  change needed" must be an intentional decision, not an omission.
- **Client telemetry (#439) goes through `otlp-collector-oidc`**, the estate's
  reference ingest image, one instance per environment (`deploy/docker-compose.yml`,
  routed by Traefik on OTLP's own `/v1/` paths of the app's host). The app does
  not proxy telemetry; it only tells signed-in clients where to send it
  (`GET /api/telemetry/config`, from `client-telemetry/endpoint` — unset is off).
  Two things to know before touching it:
  - **The SPA now holds an OIDC access token in JavaScript.** The image accepts
    only a bearer, and the SPA's session is an `HttpOnly` cookie, so the config
    route hands the cookie's token to the page. That is the deliberate cost of
    adopting the reference ingest: script injection in the page can now read a
    token it could previously only cause to be sent. **What bounds that is the
    SPA's Content-Security-Policy (#444)**, served by the app itself
    (`crates/server/src/csp.rs`; `docs/DEPLOY.md` → SPA Content-Security-Policy):
    `script-src 'self' 'wasm-unsafe-eval'` plus the hash of Trunk's inline
    bootstrap, `connect-src 'self'` plus the telemetry endpoint's origin. Every
    e2e spec imports `test` from `e2e/csp-guard.ts`, which fails the test on any
    violation — a new script, `fetch` target or inline handler must fit the
    policy, and widening the policy is a token-exposure review. **Never put
    `security-headers@docker` (or any Traefik middleware that sets a CSP) back
    on the app router**: Traefik replaces the app's header, silently.
  - **The image is pinned and unproven.** v-note is its first real deployment.
    Every behaviour v-note relies on is asserted against the pinned tag in
    `e2e/tests/client-telemetry.spec.ts`; a gap is fixed upstream in
    `vcheesbrough/otlp-collector-oidc` and re-pinned, **never** worked around
    here (no proxy, no rewriting in the app). Bump the pin in both compose files.
- **Telemetry deviations from the cross-repo `observability` contract**
  (recorded per its §2, "Deviations are recorded"; written down by #417, which
  changed none of them). v-note predates the contract; this list is what
  grandfathers it, and each line says what closing it would take:
  1. **Metrics are scraped, not pushed.** `/metrics` on `:9090` plus the shared
     Prometheus label contract (Docker `observability.*` labels). Closing it
     needs an OTLP metric exporter, a stable `service.instance.id` (absent
     today), and the collector promoting the resource to series labels.
  2. **Telemetry is configured by product keys, not the standard `OTEL_*`
     variables** — `VNOTE__OBSERVABILITY__OTLP-*`, and the `otlp-log-filter`
     #417 added. Closing it means reading the standard variables instead; a
     per-signal log filter has no standard variable, so that one key stays
     whatever happens.
  3. **The platform selects on `log_source`, not the contract's
     `telemetry_source`.** The server stamps the contract's key
     (`telemetry_source = "otlp"`, #417) and the shared Alloy copies it into
     the indexed `log_source` (mini-config #47); client data is stamped
     `telemetry_source = "client"` by the otlp-collector-oidc ingest (#439)
     and translated the same way. Values match the contract.
     Closing it means mini-config's Loki indexing `telemetry_source` and every
     selector, dashboard and e2e spec moving to it.
  4. **`deployment.environment`, not `deployment.environment.name`**, on every
     signal from the server (client data already carries the contract's
     `deployment.environment.name`, which the shared Alloy copies into
     `deployment.environment`, #439). The rename reaches the dashboard's
     `deployment_environment` filter and stored queries.
  5. **Stdout is a second egress** alongside OTLP: every server log line
     reaches Loki twice, as `log_source="docker"` and `"otlp"`. Deliberate —
     stdout is crash-safe and carries pre-init and post-shutdown output.
     Dropping the Docker copy at the collector is the aligned end state.
  6. **Nothing counts dropped telemetry.** The Rust SDK exports nothing about
     itself; a failed or dropped batch shows only as a `warn` on the
     `opentelemetry*` targets on stdout. Closing it means wrapping the
     exporters to count what they drop and exporting that counter.
- Until push CI exists, run **local** sanity checks when you touch code
  (`cargo check`, `trunk build`, Gradle tasks) — only after those trees exist.

---

## 3. PR merge procedure (repo specifics)

Run the **`pr-review-loop`** skill (baseline §5: Part A hands the review to the
`pr-self-review` subagent in a clean context, then the one-comment-at-a-time
triage loop; treat remote review agents — Woodpecker `pr-review`, Cursor
Automation — as supplementary and unreliable, and note that `pr-review` is
currently disabled outright, see §2). Skill parameters: `OWNER=vcheesbrough`,
`REPO=v-note`. Review criteria are the baseline five plus the E2E policy in §2 —
there is no repo rubric file. The squash-merge + branch-cleanup below is
repo-specific and runs at ship time (not part of the skill).

When merging a PR to **`master`** (user request or iteration ship), **always
squash**, then **delete the feature branch** locally and on the remote:

```bash
gh pr merge <N> --squash --subject "<title>" --body "<summary>"
# branch name from: gh pr view <N> --json headRefName
git checkout master && git pull origin master
git branch -d <headRefName>
git push origin --delete <headRefName>
```

- **Do not** use merge commits (`--merge`) or rebase merge (`--rebase`) unless
  the user **explicitly** overrides for that PR.
- Squash keeps **one commit per iteration/PR** on `master`, aligned with
  trunk-based flow and semver iteration boundaries.
- **Branch cleanup is part of ship** — do not leave merged `feat/iteration-*`
  branches on the remote or locally unless the user asks to keep them.

---

## 4. What not to assume during bootstrap

Until [`docs/PLAN.md`](docs/PLAN.md) and the user say otherwise, **do not
create**:

- Cargo workspace / Rust crates
- `android/`, `frontend/`, `deploy/`, Woodpecker build/deploy pipelines,
  docker-compose, Kotlin application code
- Authentik blueprint or e2e harness

Minimal repo hygiene (**`AGENTS.md`**, **`README.md`**, **`.gitignore`**, plan
updates) is in scope for bootstrap; the full monorepo scaffold is a **separate**
plan todo.


