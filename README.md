# v-note

Self-hosted ink note capture and sync: native Android stylus authoring, owner-only pages, realtime replay on a read-only web SPA, and handwriting search over private LAN or mesh.

## Status

**Planning phase** — product and stack decisions are locked in the plan; **application code and monorepo scaffold are not started yet**. **Task queue:** [v-notes Kanban board](https://bored.desync.link/boards/v-notes). **PR review agent** runs on pull requests via [`.woodpecker/pr-review.yml`](.woodpecker/pr-review.yml) ([setup](docs/PR-AGENT.md)). Full build/e2e/deploy CI is **still deferred** (Kotlin/Rust CI TBD).

## Documentation

| Doc | Purpose |
| --- | --- |
| **[docs/PLAN.md](docs/PLAN.md)** | Product spec, stack decisions, todos — **single source of truth** |
| **[AGENTS.md](AGENTS.md)** | Agent working rules (v-notes Kanban workflow, plan alignment, git safety, PR agent + deferred full CI) |
| **[docs/PR-AGENT.md](docs/PR-AGENT.md)** | Woodpecker PR review agent — secrets, trigger, GitHub App |

## License

**AGPL-3.0-or-later** — see [LICENSE](LICENSE).
