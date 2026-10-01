#!/usr/bin/env bash
# Validate the shape of .woodpecker/deploy.yml without running it.
#
# The deploy workflow has two paths in one file, chosen per step by `when`, and
# nothing else reads that file until Woodpecker does — so a step that drifts
# onto the wrong event is found by its side effects on shared infrastructure.
# This enforces:
#   1. a push never touches dev (#462). The push path is exactly
#      compute-version → verify-release-images → tag-release; every step that
#      applies the blueprint, attaches the outpost, deploys, smokes or publishes
#      the dashboard runs on a `deployment` to target `dev` and nothing else.
#      Parallel iterations then build without queueing behind, or overwriting,
#      each other's dev deploy;
#   2. a deployment rolls out only a commit a green push pipeline tagged:
#      verify-release-images runs on both events, refuses an un-reused tag on a
#      deployment, and every step that changes dev depends on it;
#   3. no step depends on a step that runs on fewer events than it does.
#      Woodpecker prunes steps whose `when` does not match, so such a dependency
#      would name a step that no longer exists and fail the whole config.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - "$@" <<'PY'
import sys
import yaml

failures = []


def check(ok, message):
    print(("ok   " if ok else "FAIL ") + message)
    if not ok:
        failures.append(message)


with open(".woodpecker/deploy.yml") as handle:
    deploy = yaml.safe_load(handle)

WORKFLOW_EVENTS = set()
for rule in deploy.get("when", []):
    events = rule.get("event", [])
    WORKFLOW_EVENTS |= set([events] if isinstance(events, str) else events)
check(
    WORKFLOW_EVENTS == {"push", "deployment"},
    "the workflow runs on push and deployment only",
)

steps = deploy["steps"]


def events(step):
    """The events a step runs on: its own `when`, else the workflow's."""
    rules = step.get("when")
    if not rules:
        return set(WORKFLOW_EVENTS)
    found = set()
    for rule in rules:
        value = rule.get("event", sorted(WORKFLOW_EVENTS))
        found |= set([value] if isinstance(value, str) else value)
    return found


def targets_dev(step):
    return all(
        rule.get("evaluate") == 'CI_PIPELINE_DEPLOY_TARGET == "dev"'
        for rule in step.get("when", [])
    ) and bool(step.get("when"))


print("==> a push builds, verifies and tags — it never touches dev (#462)")
PUSH_PATH = {"compute-version", "verify-release-images", "tag-release"}
push_steps = {name for name, step in steps.items() if "push" in events(step)}
check(
    push_steps == PUSH_PATH,
    f"push runs exactly {sorted(PUSH_PATH)} (got {sorted(push_steps)})",
)
check(
    steps.get("tag-release", {}).get("settings", {}).get("mode") == "push-tag",
    "tag-release pushes the release tag",
)
check(
    "verify-release-images" in steps.get("tag-release", {}).get("depends_on", []),
    "tag-release waits for verify-release-images",
)

DEV_CHANGING = {
    "apply-authentik-blueprint-dev",
    "attach-sqltool-outpost-dev",
    "deploy-dev",
    "smoke-oidc-login-dev",
    "smoke-sql-console-dev",
    "smoke-web-live-dev",
    "publish-grafana-dashboard",
}
for name in sorted(DEV_CHANGING):
    step = steps.get(name)
    check(step is not None, f"{name} exists")
    if step is None:
        continue
    check(events(step) == {"deployment"}, f"{name} runs on deployment only")
    check(targets_dev(step), f"{name} runs only for target dev")

# Anything else touching shared infrastructure must be caught too, not only the
# steps named above: the blueprint plugin, the deploy image, the docker socket
# outside the read-only verify step, or a secret that writes somewhere.
WRITE_SECRETS = {"authentik_api_token", "grafana_api_token", "v_note_devops_sovereign_access_url"}


def secrets(step):
    found = set()
    for block in (step.get("environment") or {}, step.get("settings") or {}):
        stack = [block]
        while stack:
            node = stack.pop()
            if isinstance(node, dict):
                if "from_secret" in node:
                    found.add(node["from_secret"])
                stack.extend(node.values())
    return found


for name in sorted(push_steps):
    step = steps[name]
    image = step.get("image", "")
    check(
        "authentik-blueprint" not in image and "sovereign-config-cli" not in image,
        f"push step {name} uses neither the blueprint plugin nor the deploy image",
    )
    check(
        not (secrets(step) & WRITE_SECRETS),
        f"push step {name} holds no Authentik, Grafana or deploy credential",
    )

print("==> a deployment rolls out only a commit a green push tagged")
verify = steps.get("verify-release-images", {})
check(
    events(verify) == {"push", "deployment"},
    "verify-release-images runs on push and deployment",
)
commands = "\n".join(verify.get("commands", []))
check(
    ".release-tag-reused" in commands and "deployment" in commands,
    "verify-release-images refuses a deployment of an untagged commit",
)


def ancestors(name, seen=None):
    seen = set() if seen is None else seen
    for dep in steps.get(name, {}).get("depends_on", []) or []:
        if dep not in seen:
            seen.add(dep)
            ancestors(dep, seen)
    return seen


for name in sorted(DEV_CHANGING):
    check(
        "verify-release-images" in ancestors(name),
        f"{name} runs after verify-release-images",
    )
    check(
        "validate-deployment" in ancestors(name),
        f"{name} runs after validate-deployment",
    )
check(
    not any(steps[n].get("settings", {}).get("mode") == "push-tag"
            for n in steps if "deployment" in events(steps[n])),
    "the deployment path pushes no tag (a deployable commit is already tagged)",
)

print("==> no step depends on one pruned from its own events")
for name, step in steps.items():
    for dep in step.get("depends_on", []) or []:
        check(dep in steps, f"{name}: dependency {dep} exists")
        if dep in steps:
            check(
                events(step) <= events(steps[dep]),
                f"{name}: {dep} runs on every event {name} does",
            )

if failures:
    print(f"\ndeploy pipeline validation FAILED ({len(failures)} check(s))")
    sys.exit(1)
print("\ndeploy pipeline OK")
PY
