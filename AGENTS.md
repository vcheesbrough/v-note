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
| **Phase** | **Planning / minimal bootstrap** — no application code yet; full scaffold deferred until more planning |

**Stack summary:** Rust (Axum) server, PostgreSQL, Leptos/Trunk SPA, native Kotlin (Compose) Android, Authentik OIDC, bored-aligned deploy intent. Details and locked decisions are in [`docs/PLAN.md`](docs/PLAN.md) — treat that document as the spec source of truth, not this file.

**Kanban board:** **[v-notes](https://bored.desync.link/boards/v-notes)** — task queue for this repo (not the bored product board).

**Spec:** Product and stack decisions live in [`docs/PLAN.md`](docs/PLAN.md); cards should stay aligned with the plan.

---

## 1. Kanban / bored card workflow

Task queue for **v-note** lives on the bored **v-notes** board: **https://bored.desync.link/boards/v-notes**

When this project (or you) uses **Kanban cards** as the task queue:

1. **One card at a time** unless the user explicitly says otherwise (no parallel cards unless told).

2. **Start work:** As soon as you pick up a card, **move it to the In Progress column** on the **v-notes** board using **bored MCP** (`list_boards` → find **v-notes** → `list_columns` → **`move_card`**). Resolve the card with **`get_card_by_number`** when you only know `#N`.

3. **Iteration in the card title (this repo):** Bored cards have **no separate title field** — the board shows the **first markdown `#` heading** in **`body`**.
   - **Todo / backlog:** Use a **plain descriptive** heading only — **do not** write **`# Iteration N — …`** yet (**N** is unknown until work starts).
   - **In progress:** Right after **`move_card`** into **In progress**, **`update_card`** so the first `#` line becomes **`# Iteration N — …`** where **N** is the iteration you are committing to for this card (same **N** as **`feat/iteration-N-…`** branch at **start of work**; when a workspace **`Cargo.toml`** exists, align minor **1.N.x** with that **N**). One prefix; avoid doubling.

4. **Reconcile with reality:** **Compare the card body and [`docs/PLAN.md`](docs/PLAN.md) to the current source tree**, **replan** if scope or facts drifted, then **`update_card`** (and update the plan if needed) so both stay accurate (acceptance, files, out-of-scope notes). Preserve **`# Iteration N — …`** once set; if **N** changes mid-flight, **`update_card`** with the new heading.

5. **Branches:** After **N** is fixed (**In progress** per §3), implement on **`feat/iteration-N-short-slug`** from **`main`** (default trunk).
   **Never** create follow-on branches **from** the feature branch — always branch from **`main`**, one iteration branch per card.

6. **Ship:** When the **PR is merged**, **move the card to Done** via bored MCP (**`move_card`** into the Done column). Update **`docs/PLAN.md`** todos if the work closes a planned item.

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

**Full CI still deferred:** push build, e2e, Kotlin/Rust CI, deploy compose, and fixture-validation pipelines are **not** in the tree yet (planned: bored-aligned Woodpecker — see **Stack decisions** in the plan).

- Do **not** add `.woodpecker/build.yml`, deploy compose, or CI fixtures unless the user requests that slice.
- When full CI lands, extend this section with post-push verification obligations (mirror bored `AGENTS.md` §2).
- Until then, run **local** sanity checks only when you touch code (`cargo check`, `trunk build`, Gradle tasks, etc.) — and only after those trees exist.

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

When the user asks to **raise a PR**, or when a PR has **unresolved review comments** (from the [`.woodpecker/pr-review.yml`](.woodpecker/pr-review.yml) Claude PR agent or a human reviewer):

1. **Detect / open the PR** — push the feature branch from **`main`**, open with `gh pr create` if needed.
2. **Fetch unresolved threads** via GitHub GraphQL (`reviewThreads` where `isResolved == false`).
3. **Present one comment at a time** — file, author, analysis, suggested fix; **the user decides** (include ignore / push back).
4. **Apply chosen resolutions locally** — sanity-check, but **do not commit** until the user approves a batch.
5. **Reply on the PR thread** and resolve threads when the user picks a fix or explicit won't-do.
6. **One commit per batch** when the user asks to commit addressed comments.

**Hard rules:** User decides every comment; one comment per decision prompt; no auto-resolve on "discuss further"; audit trail stays on the PR.

**CI after push:** PR review agent runs automatically on pull requests (§3). Full build/e2e CI is not required until that slice lands. When full CI exists, verify green status before declaring a batch done.

---

## 7. What not to assume during bootstrap

Until **`docs/PLAN.md`** and the user say otherwise, **do not create**:

- Cargo workspace / Rust crates
- `android/`, `frontend/`, `deploy/`, Woodpecker build/deploy pipelines, docker-compose, Kotlin application code
- Authentik blueprint or e2e harness

Minimal repo hygiene (**`AGENTS.md`**, **`README.md`**, **`.gitignore`**, plan updates) is in scope for bootstrap; full monorepo scaffold is a **separate** plan todo.
