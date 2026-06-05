# v-note

Self-hosted ink note capture and sync: native Android stylus authoring, owner-only pages, realtime replay on a read-only web SPA, and handwriting search over private LAN or mesh.

## Status

**Planning phase** — product and stack decisions are locked in the plan; **application code and monorepo scaffold are not started yet** (next card: **#145**). **Task queue:** [v-notes Kanban board](https://bored.desync.link/boards/v-notes). **Semver:** pre-MVP **`0.N.P`** → **`1.0.0`** at MVP (**#151**). **PR review agent** runs on pull requests via [`.woodpecker/pr-review.yml`](.woodpecker/pr-review.yml) ([setup](docs/PR-AGENT.md)). Full build/e2e/deploy CI lands with **#145**.

## Documentation

| Doc | Purpose |
| --- | --- |
| **[docs/PLAN.md](docs/PLAN.md)** | Product spec, stack decisions, engineering workflows — **single source of truth** |
| **[AGENTS.md](AGENTS.md)** | Agent working rules (Kanban, semver `0.N.P` pre-MVP → `1.0.0` at MVP, CI, git safety, PRs) |
| **[docs/DEV.md](docs/DEV.md)** | Local development and CI reproduction (after **#145**) |
| **[docs/DEPLOY.md](docs/DEPLOY.md)** | Woodpecker deploy, secrets, image tags (skeleton until **#145**) |
| **[docs/PR-AGENT.md](docs/PR-AGENT.md)** | Woodpecker PR review agent — secrets, trigger, GitHub App |

## License

**AGPL-3.0-or-later** — see [LICENSE](LICENSE).
