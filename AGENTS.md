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

- **PR review agent (bootstrapped):** Woodpecker runs [`.woodpecker/pr-review.yml`](.woodpecker/pr-review.yml)
  on every PR — Claude PR agent via [claude-pr-agent](https://github.com/vcheesbrough/claude-pr-agent).
  Repo prompt: [`.woodpecker/pr-review-prompt.md`](.woodpecker/pr-review-prompt.md).
  Secrets/setup: [`docs/PR-AGENT.md`](docs/PR-AGENT.md).
- **Push CI is four Woodpecker workflows** in [`.woodpecker/`](.woodpecker/):
  `checks` (lint, rust-test, deploy-script-validation, android-build-box-pin),
  `web` (build-web → e2e-web) and `android` (build-android, API 29/36
  instrumented) run in parallel; `deploy` (verify-release-images → blueprint →
  auto-deploy-dev → tag) runs only when all three succeed. **`ci-watch` must follow
  every workflow of the pushed commit's pipeline to completion** — one green
  workflow while another is still running is not a result. Do not fold them back
  into one file: Woodpecker runs step `depends_on` as whole stages, so steps in one
  workflow wait on unrelated slow steps (#320). Reproduce commands per
  [`docs/DEV.md`](docs/DEV.md):
  - `just rust-ci lint` / `just rust-ci test` (`Dockerfile.rust-ci`, as the `lint` / `rust-test` steps)
  - `docker build -f Dockerfile.web -t v-note:ci-local --secret id=github_token,env=GITHUB_TOKEN .`
  - `TEST_IMAGE=v-note:ci-local docker compose -f e2e/docker-compose.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright`
  - **All push workflows, including `e2e-web` and both Android lanes, must be green** before an iteration is done.
- **E2E policy (locked):** every **user-facing feature** in an iteration card
  must have **automated e2e tests in CI** before that card merges (see
  [`docs/PLAN.md`](docs/PLAN.md) **E2E testing**). Contract/unit tests
  supplement, they do not replace e2e. This **strengthens** baseline §6 — no
  manual-only or runbook-only acceptance for product behaviour.
- **Observability policy:** for every implementation ticket, consciously decide
  whether the change needs updates to **metrics, logs, spans/traces,
  trace-log correlation, labels, dashboards, alerts, or runbook/docs**. "No
  change needed" must be an intentional decision, not an omission.
- Until push CI exists, run **local** sanity checks when you touch code
  (`cargo check`, `trunk build`, Gradle tasks) — only after those trees exist.

---

## 3. PR merge procedure (repo specifics)

Run the **`pr-review-loop`** skill (baseline §5: self-review every PR you open,
then the one-comment-at-a-time triage loop; treat remote review agents —
Woodpecker `pr-review`, Cursor Automation — as supplementary and unreliable).
Skill parameters: `OWNER=vcheesbrough`, `REPO=v-note`, rubric
[`.woodpecker/pr-review-prompt.md`](.woodpecker/pr-review-prompt.md). The
squash-merge + branch-cleanup below is repo-specific and runs at ship time (not
part of the skill).

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
