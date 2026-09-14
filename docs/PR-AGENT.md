# PR review agent

Automated pull-request reviews via [claude-pr-agent](https://github.com/vcheesbrough/claude-pr-agent), triggered by Woodpecker on every GitHub pull request.

## Pipeline

| File | Purpose |
| --- | --- |
| [`.woodpecker/pr-review.yml`](../.woodpecker/pr-review.yml) | Woodpecker pipeline — runs on `pull_request` events |
| [`.woodpecker/pr-review-prompt.md`](../.woodpecker/pr-review-prompt.md) | Repo-local review rules (stack, MVP constraints, output JSON schema) |

This pipeline is **standalone**. It does not depend on the push workflows (`.woodpecker/checks.yml`, `web.yml`, `android.yml`, `deploy.yml`) or any docker build/e2e CI.

## Trigger

Woodpecker runs the `review` step when:

- **Event:** `pull_request` (open, sync, reopen)
- **Repo:** `vcheesbrough/v-note` must be registered in Woodpecker and have this pipeline enabled

The agent fetches the PR diff via the GitHub API, runs Claude Code headless with the repo prompt, and posts a GitHub review (summary + optional inline comments).

## Required secrets

Woodpecker YAML uses `from_secret: <name>`. Values are **not** stored in Woodpecker itself — they are fetched from **OpenBao** via the Woodpecker secret extension ([woodpecker-openbao-broker](https://github.com/vcheesbrough/woodpecker-openbao-broker)). Add or rotate values under the path your broker maps for **`vcheesbrough/v-note`** (same pattern as bored: `secret/woodpecker/repos/vcheesbrough/<repo>`).

| Secret | Env var in container | Purpose |
| --- | --- | --- |
| `claude_oauth_token` | `CLAUDE_CODE_OAUTH_TOKEN` | Claude Code OAuth token (subscription) |
| `pr_reviewer_gh_app_id` | `PR_REVIEWER_GH_APP_ID` | GitHub App ID for PR posting |
| `pr_reviewer_gh_app_installation_id` | `PR_REVIEWER_GH_APP_INSTALLATION_ID` | Installation ID for this org/repo |
| `pr_reviewer_gh_app_private_key_b64` | `PR_REVIEWER_GH_APP_PRIVATE_KEY_B64` | Base64-encoded PEM private key |

**Woodpecker / OpenBao:** mark these secrets **pull_request-allowed** so fork PRs are handled per your security policy (same as bored).

**Optional:** `ANTHROPIC_API_KEY` inside the image takes precedence over `CLAUDE_CODE_OAUTH_TOKEN` if set — bored uses OAuth only.

## GitHub App

The reviewer authenticates as a **GitHub App** (not a PAT). The app must be **installed on `vcheesbrough/v-note`** with at least:

- Pull requests: read & write (post reviews and comments)
- Contents: read (fetch changed files)
- Metadata: read

If v-note shares the same app as bored/mini-config, add a **new installation** or extend the existing installation to include `vcheesbrough/v-note` and use the matching `pr_reviewer_gh_app_installation_id` for this repo’s OpenBao path.

## Disabling quickly

1. Rename `.woodpecker/pr-review.yml` → `.woodpecker/pr-review.yml.disabled` and push, or
2. Remove any of the four secrets — the step fails fast.

## Agent workflow in Cursor / Claude Code

When triaging PR comments from this bot, follow the PR comment loop in [`AGENTS.md`](../AGENTS.md) §5.
