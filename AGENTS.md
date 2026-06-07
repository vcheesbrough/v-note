# Agent guide — v-note

This file is the single source of truth for AI agents and assistants working in this repo. It is read natively by Cursor, Claude Code, Codex, Aider, Jules, Amp, etc.

**Product, stack, and architecture** live in [`docs/PLAN.md`](docs/PLAN.md). **Repository overview** is in [`README.md`](README.md). This file is only the working-rules layer.

If you change a rule below, change it here — there is no parallel copy.

---

## Repository context

| Item | Value |
| --- | --- |
| **Local path** | `/home/vincent/dev/v-note` |
| **Remote** | [vcheesbrough/v-note](https://github.com/vcheesbrough/v-note) |
| **Layout** | **Monorepo** (server, SPA, Android, deploy, schemas — when scaffolded) |
| **License** | **AGPL-3.0-or-later** — see [`LICENSE`](LICENSE) |
| **Phase** | **Minimal bootstrap done** — planning locked; **#145** lands monorepo scaffold + CI; no application code yet |

**Stack summary:** Rust (Axum) server, PostgreSQL, Leptos/Trunk SPA, native Kotlin (Compose) Android, Authentik OIDC, bored-aligned deploy intent. Details and locked decisions are in [`docs/PLAN.md`](docs/PLAN.md) — treat that document as the spec source of truth, not this file.

**Kanban board:** **[v-notes](https://bored.desync.link/boards/v-notes)** — task queue for this repo (not the bored product board).

**Spec:** Product and stack decisions live in [`docs/PLAN.md`](docs/PLAN.md); cards should stay aligned with the plan.

---

## 1. Kanban / bored card workflow

Task queue for **v-note** lives on the bored **v-notes** board: **https://bored.desync.link/boards/v-notes**

When this project (or you) uses **Kanban cards** as the task queue:

1. **One card at a time** unless the user explicitly says otherwise (no parallel cards unless told).

2. **Start work:** As soon as you pick up a card, **move it to the In Progress column** on the **v-notes** board using **bored MCP** (`list_boards` → find **v-notes** → `list_columns` → **`move_card`**). Resolve the card with **`get_card_by_number`** when you only know `#N`.

3. **Iterations — one card, one branch, one N:** Each iteration is exactly **one bored card** and **one feature branch**. No stacking branches; do not split one iteration across multiple cards or branches.
   - **Sequential N:** Assign iteration numbers **only when work starts**, in strict order. The first card moved to **In progress** is **N=1**, the next started card is **N=2**, and so on. To pick the next **N**, inspect **Done** and **In Progress** cards on the board and use **highest existing N + 1** (or **1** if none exist yet).
   - **Semver** (locked — see [`docs/PLAN.md`](docs/PLAN.md) **Engineering workflows** → **Versioning**): **minor `N` always = iteration number** (1, 2, 3… globally, assigned only when work starts). **Major** signals **phase**, not card count — **do not assume** a fixed number of pre-MVP iterations; new MVP backbone cards may be inserted before **`vertical-slice-mvp`** closes:
     - **Pre-MVP** (before **`vertical-slice-mvp`** closes on **`main`**): **`0.N.P`** — e.g. first iteration → **`0.1.0`**, fifth → **`0.5.0`** (count grows with however many iterations ship).
     - **MVP release:** when the **MVP completion card** (**#151** today) merges to **`main`**, set workspace to **`1.0.0`** and tag **`v1.0.0`** (first stable MVP).
     - **Post-MVP** (after **`1.0.0`** on **`main`**): **`1.N.P`** — **`N` continues** from the last pre-MVP iteration (e.g. if MVP shipped as iteration 9, next post-MVP card is **`1.10.0`**).
     - **Patch `P`:** bump only on the active iteration branch; reset **`P` to 0** at iteration start (`0.N.0` or `1.N.0`).
     - When **`Cargo.toml`** exists, set version at **In progress** per rules above.

4. **Card title and body (no iteration until start):** Bored cards have **no separate title field** — the board shows the **first markdown `#` heading** in **`body`**.
   - **Todo / backlog:** **Plain descriptive heading only** — e.g. `# Bootstrap monorepo, CI, and test infrastructure`. Do **not** use **`# Iteration N — …`**, do **not** name **`feat/iteration-N-…`** branches, and do **not** write semver in the card body.
   - **In progress:** Immediately after **`move_card`**, **`update_card`**:
     1. Set the first `#` line to **`# Iteration N — …`** (one prefix; keep the descriptive title after the em dash).
     2. Record **Branch:** `feat/iteration-N-short-slug` from **`main`** in the card body.
     3. Record **Version:** `0.N.0` (pre-MVP — before **`vertical-slice-mvp`** closes) or `1.N.0` (post-MVP — after **`1.0.0`** on **`main`**) in the card body when a workspace exists. **Exception:** MVP completion card merge sets **`1.0.0`** on **`main`** (today **#151**).
   - **Done:** Keep **`# Iteration N — …`** — N is historical record.

5. **Reconcile with reality:** **Compare the card body and [`docs/PLAN.md`](docs/PLAN.md) to the current source tree**, **replan** if scope or facts drifted, then **`update_card`** (and update the plan if needed) so both stay accurate (acceptance, files, out-of-scope notes). **New MVP iterations** discovered before release → **`create_card`** in TODO (correct order), update **Kanban card map** in the plan — do **not** assume the backbone stays **#145–#151**. Preserve **`# Iteration N — …`** once set; if **N** must change mid-flight (rare), **`update_card`** with the new heading and semver/branch notes.

6. **Branches:** Create **`feat/iteration-N-short-slug`** from **`main`** only **after** **N** is assigned (**In progress** per §4). **Never** branch from another feature branch.

7. **Ship:** When the **PR is merged**, **move the card to Done** via bored MCP (**`move_card`** into the Done column). Update **`docs/PLAN.md`** todos if the work closes a planned item.

### Planning vs implementation (scope gate)

Cards in **TODO / backlog** are **spec only** until the card moves to **In progress** (§2–4). Do **not** treat runbook or design discussion as permission to edit the repo.

| User intent | Allowed edits | Not allowed |
| --- | --- | --- |
| Refine / update a **TODO** card (runbook, acceptance, order) | **bored MCP** `update_card` (and **`docs/PLAN.md`** only if the user asks for plan alignment) | Source code, CI, scripts, compose, new files, commits |
| **Start iteration** — e.g. *pick up #N*, *implement #N*, *start work*, *open PR* | Full implementation per the active card + §1 branch/version rules | Scope beyond the card without replanning |
| Ambiguous — shaping a card vs building it | **Ask once:** *“Ticket only, or implement?”* then follow the answer | Guessing and coding “helpfully” |

**While a card is TODO:**

- The **card body** is the source of truth for acceptance, operator runbook, expected paths, and out-of-scope — not the repo.
- **One deliverable per request** — ticket update **or** plan update **or** implementation; not all three unless the user asks for each.
- **Verify before writing** facts into a card or plan: read **this repo’s** pipeline, plugins, and scripts (e.g. `.woodpecker/build.yml`, [`woodpecker-plugin-release-versions`](https://github.com/vcheesbrough/woodpecker-plugin-release-versions)). Do **not** copy formats or behaviour from **bored** or other repos unless v-note uses the same mechanism.
- Use placeholders like **`{release}`** in cards and define them once (v-note: Woodpecker **`compute-version`** → **`.release-tag`**, plain semver **`MAJOR.MINOR.PATCH`** — not `semver-<sha>` unless the pipeline changes).

**If you overstepped** (repo edited when only the card should change): revert repo changes immediately; keep or fix the card; do not “finish” the implementation in the same turn unless the user then asks to start work.

### Bored — MCP only

- Use **bored MCP tools** for **all** board/column/card reads and writes on the **v-notes** board: `list_boards`, `get_board`, `list_columns`, `list_cards`, `get_card`, **`get_card_by_number`**, `create_card`, **`update_card`**, **`move_card`**, `delete_*`, `reorder_columns`, etc.
- **Do not** call the bored HTTP API with `curl`, scripts, or ad-hoc clients unless MCP is broken or unavailable — then say so once, fall back briefly, and still obey **one card**, trunk-based branches, **no stacking branches**.
- **Default endpoint:** **`https://bored.desync.link`** with scope **`bored:prod:access`** unless the user instructs otherwise. Rebuild **`cargo build -p mcp --release`** in the **[bored](/home/vincent/dev/bored)** repo when MCP **code** changes. Do not add **`bored-dev`** / duplicate **`bored`** server entries on your own initiative.

---

## 2. Plan alignment

1. **Read the plan first.** Before significant work, skim [`docs/PLAN.md`](docs/PLAN.md) — especially **Project todos**, **Stack decisions**, and **Decisions made**.

2. **Reconcile with reality.** Compare the plan, active Kanban card, and any user request to the **current source tree**. If scope or facts drifted, **`update` `docs/PLAN.md`** and/or the card so they stay accurate. Do not implement against a stale spec silently.

3. **Commits and pushes:** Only commit or push when the **user explicitly asks**. Do not invent CI or deploy steps that are not in the tree yet.

---

## 3. CI / pipelines

**PR review agent (bootstrapped):** Woodpecker runs [`.woodpecker/pr-review.yml`](.woodpecker/pr-review.yml) on every pull request — Claude PR agent via [claude-pr-agent](https://github.com/vcheesbrough/claude-pr-agent). Repo prompt: [`.woodpecker/pr-review-prompt.md`](.woodpecker/pr-review-prompt.md). Secrets and setup: [`docs/PR-AGENT.md`](docs/PR-AGENT.md).

**Full push/deploy CI:** lands with **#145** — [`.woodpecker/build.yml`](.woodpecker/build.yml), deploy compose, contract-validation, e2e (see [`docs/PLAN.md`](docs/PLAN.md) **Engineering workflows**). Until that scaffold exists, do **not** add pipeline files unless the user requests that slice.

- **E2E policy (locked):** Every **user-facing feature** in an iteration card must have **automated e2e tests in CI** before that card merges — see [`docs/PLAN.md`](docs/PLAN.md) **E2E testing**. Contract/unit tests supplement; they do not replace e2e. No manual-only or runbook-only acceptance for product behaviour.
- Until push CI exists, run **local** sanity checks when you touch code (`cargo check`, `trunk build`, Gradle tasks, etc.) — only after those trees exist.

### Monitor builds (agent obligation)

Whenever you **trigger or depend on a remote build** (git push, Woodpecker pipeline, CI-context `docker build`, etc.), **monitor it until completion**. Poll GitHub commit status for the relevant SHA (`gh api repos/vcheesbrough/v-note/commits/$SHA/status`) and/or **Woodpecker MCP** when available. Report the outcome: which checks ran, and whether the combined state is **success**, **failure**, or still **pending**. If status stays **pending**, say so and that monitoring should continue — do not invent a final result.

### Woodpecker / CI after every push

When this repo has been pushed (or the user asks to verify CI) **and** [`.woodpecker/build.yml`](.woodpecker/build.yml) exists:

1. **Confirm pipeline outcome** for that commit (Woodpecker → GitHub status):
   - `SHA=$(git rev-parse HEAD)`
   - `gh api repos/vcheesbrough/v-note/commits/$SHA/status --jq '.state'` → expect **`success`**.
   - Optional: `gh api repos/vcheesbrough/v-note/commits/$SHA/status --jq '.statuses[] | "\(.context): \(.state)"'`

2. **If anything failed**, reproduce locally per [`docs/DEV.md`](docs/DEV.md) and [`.woodpecker/build.yml`](.woodpecker/build.yml):
   - `docker build -t v-note:ci-local .` (rustfmt / clippy / tests / builds inside Dockerfile when wired).
   - `TEST_IMAGE=v-note:ci-local docker compose -f e2e/docker-compose.test.yml up --build --force-recreate --abort-on-container-exit --exit-code-from playwright`

3. **Fix failures** in-repo, commit (when user asks), push, **poll status again** until green (see **Monitor builds** above). **All push steps including `e2e` must be green** before declaring an iteration done.

If `gh` is unavailable, say so once and use Woodpecker MCP or the Woodpecker UI when you can. While status is **pending**, report that and continue monitoring when the user wants a live update.

---

## 4. Git safety

- **Never** update git config.
- **Never** run destructive git commands (`push --force`, `reset --hard`, etc.) unless the user explicitly requests them.
- **Never** force-push to **`main`** / **`master`**.
- **Never** skip hooks (`--no-verify`) unless the user explicitly requests it.
- Prefer **trunk-based** flow: feature branch → PR → merge to **`main`**.
- **Do not commit** unless the user asks.

---

## 5. MCP and secrets

These apply when using MCP tools in Cursor or Claude Code:

- **Never paste secrets** from `~/.cursor/mcp.json`, `~/.claude.json`, or any other config into chat.
- On MCP connection failures, fix config **wholesale** (copy `mcpServers` from Claude → Cursor in one pass) rather than hand-merging single fields — see global MCP discipline in your environment.
- **bored MCP** is used for **v-notes** board task tracking (§1) and shares the same **`bored-mcp`** binary as the bored product repo. v-note has **no repo-shipped MCP launcher** during bootstrap.

---

## 6. Pull requests and review comments

When the user asks to **raise a PR**, or when a PR has **unresolved review comments** (from self-review, the [`.woodpecker/pr-review.yml`](.woodpecker/pr-review.yml) Claude PR agent, Cursor Automation, or a human reviewer), work in order: **self-review and post to GitHub**, then **triage and resolve** every open thread the same way.

### Self-review when you open the PR (required)

Remote PR review agents (Woodpecker `pr-review`, Cursor Automation) are **unreliable** — do **not** treat them as the primary review path. When **you** raise or update a PR (push to its branch), **immediately** perform an in-session review:

1. Read [`.woodpecker/pr-review-prompt.md`](.woodpecker/pr-review-prompt.md) — same rubric the Woodpecker agent uses (correctness, security/OWASP, contracts, e2e policy, scope).
2. Review the **full PR diff** (`git diff master...HEAD` or `gh pr diff <N>`), file-by-file against those criteria. Consult `docs/PLAN.md`, `AGENTS.md`, and touched source files for context when needed.
3. **Present findings to the user** in chat before waiting on remote agents: actionable issues with **file/line**, **severity** (blocker / major / minor / nit), and **suggested fix**; include a short summary and positives where useful.
4. **Submit the same findings to the GitHub PR** (not chat-only): post a PR review with **`gh`** — summary (`gh pr review` with `--comment`, `--approve`, or `--request-changes`) plus **inline comments** on specific lines where possible (blockers and majors at minimum). Use GraphQL (`addPullRequestReview` / `addPullRequestReviewComment`) or `gh api` when needed; take **commit SHA** and **line numbers** from `gh pr diff` / the PR head commit. Minor items and nits may stay in the summary when inline placement is awkward.
5. **Then fall back to [Triage and resolve](#triage-and-resolve-comments)** — treat your submitted inline threads like any other reviewer’s: unresolved GraphQL `reviewThreads`, one-at-a-time user decisions, local fixes when approved, reply and resolve on GitHub. Do **not** skip triage because you authored the comments.

Self-review is **required** when the agent opens the PR; remote agent output is **optional/supplementary**.

### Merge PRs (required)

When merging a PR to **`master`** (user request or iteration ship), **always squash**, then **delete the feature branch** locally and on the remote:

```bash
gh pr merge <N> --squash --subject "<title>" --body "<summary>"
# branch name from: gh pr view <N> --json headRefName
git checkout master && git pull origin master
git branch -d <headRefName>
git push origin --delete <headRefName>
```

- **Do not** use merge commits (`gh pr merge --merge`) or rebase merge (`--rebase`) unless the user **explicitly** overrides for that PR.
- Squash keeps **one commit per iteration/PR** on **`master`**, aligned with trunk-based flow and semver iteration boundaries (§1).
- **Branch cleanup** is part of ship — do not leave merged `feat/iteration-*` branches on the remote or in the local checkout unless the user asks to keep them.

### Monitor after you open a PR

In parallel with triage (and after self-review is posted), **watch for supplementary feedback** from humans and remote agents. You may wait for automated review agents to finish before treating **external** review state as final — e.g. Woodpecker `pr-review`, Cursor Automation, Bugbot, or similar check contexts. Poll **`gh pr checks`** and commit statuses (pending → success/failure) and GraphQL unresolved `reviewThreads`. **Surface agent completion** when it arrives (which agent finished, pass/fail), then **surface new comments** to the user (count and brief summary). **Do not assume** there are no review threads while agent checks are still pending; re-check while CI or agents are in flight unless the user says to stop.

### Triage and resolve comments

Applies to **all** unresolved PR review threads — including those you posted in self-review, plus humans and remote agents.

1. **Detect / open the PR** — push the feature branch from **`main`**, open with `gh pr create` if needed; run **Self-review** first when you open the PR.
2. **Fetch unresolved threads** via GitHub GraphQL (`reviewThreads` where `isResolved == false`); refresh after **Monitor** surfaces new external comments.
3. **Present one comment at a time** — file, author, analysis, suggested fix; **the user decides** (include ignore / push back).
4. **Apply chosen resolutions locally** — sanity-check, but **do not commit** until the user approves a batch.
5. **Reply on the PR thread** and resolve threads when the user picks a fix or explicit won't-do.
6. **One commit per batch** when the user asks to commit addressed comments.

**Hard rules:** User decides every comment; one comment per decision prompt; no auto-resolve on "discuss further"; audit trail stays on the PR.

**CI after push:** Full build/e2e CI runs on push when [`.woodpecker/build.yml`](.woodpecker/build.yml) exists (§3). Woodpecker `pr-review` may be disabled — self-review covers that gap. Verify green push CI before declaring an iteration done.

---

## 7. What not to assume during bootstrap

Until **`docs/PLAN.md`** and the user say otherwise, **do not create**:

- Cargo workspace / Rust crates
- `android/`, `frontend/`, `deploy/`, Woodpecker build/deploy pipelines, docker-compose, Kotlin application code
- Authentik blueprint or e2e harness

Minimal repo hygiene (**`AGENTS.md`**, **`README.md`**, **`.gitignore`**, plan updates) is in scope for bootstrap; full monorepo scaffold is a **separate** plan todo.
