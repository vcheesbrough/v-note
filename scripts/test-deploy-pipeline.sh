#!/usr/bin/env bash
# Validate the shape of .woodpecker/verify-tag-deploy.yml (and the versioning of
# web.yml and android.yml) without running them.
#
# The verify-tag-deploy workflow has two paths in one file, chosen per step by `when`, and
# nothing else reads that file until Woodpecker does — so a step that drifts
# onto the wrong event is found by its side effects on shared infrastructure.
# Versioning follows the agent-shared release-flow contract through the
# release-versions plugin (#476). This enforces:
#   1. a push never touches dev (#462). The push/manual path is exactly
#      version → publish; every step that applies the blueprint, attaches the
#      outpost, deploys, smokes, tags or publishes the dashboard runs on a
#      `deployment` to target `dev` and nothing else;
#   2. a deployment ships only what `resolve` chose: every step that changes dev
#      runs after it, and nothing else writes .release-tag on that path;
#   3. the git tag means "deployed": `tag` runs only on a deployment, after
#      deploy-dev, beside the smokes (never after them);
#   4. no step depends on a step that runs on fewer events than it does.
#      Woodpecker prunes steps whose `when` does not match, so such a dependency
#      would name a step that no longer exists and fail the whole config;
#   5. web, android and verify-tag-deploy all run `version` on the same pinned
#      plugin image, so the three workflows of one pipeline agree on its
#      version, and only `publish` pushes images, after every test.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - "$@" <<'PY'
import re
import sys
import yaml

failures = []


def check(ok, message):
    print(("ok   " if ok else "FAIL ") + message)
    if not ok:
        failures.append(message)


PLUGIN = re.compile(
    r"^ghcr\.io/vcheesbrough/woodpecker-plugin-release-versions:\d+\.\d+\.\d+@sha256:[0-9a-f]{64}$"
)

with open(".woodpecker/verify-tag-deploy.yml") as handle:
    deploy = yaml.safe_load(handle)

WORKFLOW_EVENTS = set()
for rule in deploy.get("when", []):
    events = rule.get("event", [])
    WORKFLOW_EVENTS |= set([events] if isinstance(events, str) else events)
check(
    WORKFLOW_EVENTS == {"push", "manual", "deployment"},
    "the workflow runs on push, manual and deployment only",
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


def mode(step):
    return (step.get("settings") or {}).get("mode")


def ancestors(name, seen=None):
    seen = set() if seen is None else seen
    for dep in steps.get(name, {}).get("depends_on", []) or []:
        if dep not in seen:
            seen.add(dep)
            ancestors(dep, seen)
    return seen


print("==> a push builds and publishes — it never touches dev (#462)")
BUILD_PATH = {"version", "publish"}
for event in ("push", "manual"):
    on_event = {name for name, step in steps.items() if event in events(step)}
    check(
        on_event == BUILD_PATH,
        f"{event} runs exactly {sorted(BUILD_PATH)} (got {sorted(on_event)})",
    )
check(mode(steps.get("version", {})) == "version", "version runs the plugin's version mode")
publish = steps.get("publish", {})
check(mode(publish) == "publish", "publish runs the plugin's publish mode")
check("version" in (publish.get("depends_on") or []), "publish waits for version")
check(
    (publish.get("settings") or {}).get("images")
    == ["registry.desync.link/v-note", "registry.desync.link/v-note-android"],
    "publish pushes both shipped images",
)
check(
    {d.get("name") for d in deploy.get("depends_on", [])} == {"checks", "web", "android"},
    "the workflow waits for checks, web and android (publish follows every test)",
)

DEV_CHANGING = {
    "apply-authentik-blueprint-dev",
    "attach-sqltool-outpost-dev",
    "deploy-dev",
    "smoke-oidc-login-dev",
    "smoke-sql-console-dev",
    "smoke-web-live-dev",
    "publish-grafana-dashboard",
    "tag",
}
for name in sorted(DEV_CHANGING):
    step = steps.get(name)
    check(step is not None, f"{name} exists")
    if step is None:
        continue
    check(events(step) == {"deployment"}, f"{name} runs on deployment only")
    check(targets_dev(step), f"{name} runs only for target dev")

# Anything else touching shared infrastructure must be caught too, not only the
# steps named above: the blueprint plugin, the deploy image, or a secret that
# writes somewhere.
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
            elif isinstance(node, list):
                stack.extend(node)
    return found


for name in sorted(BUILD_PATH):
    step = steps.get(name, {})
    image = step.get("image", "")
    check(
        "authentik-blueprint" not in image and "sovereign-config-cli" not in image,
        f"build step {name} uses neither the blueprint plugin nor the deploy image",
    )
    check(
        not (secrets(step) & WRITE_SECRETS),
        f"build step {name} holds no Authentik, Grafana or deploy credential",
    )

print("==> a deployment ships only what resolve chose")
resolve = steps.get("resolve", {})
check(events(resolve) == {"deployment"}, "resolve runs on deployment only")
check(mode(resolve) == "resolve", "resolve runs the plugin's resolve mode")
check(
    (resolve.get("settings") or {}).get("images") == publish.get("settings", {}).get("images"),
    "resolve checks the same images publish pushes",
)
check(
    (resolve.get("settings") or {}).get("environments") == {"dev": {}},
    "resolve allows target dev only, from any branch (prod is #388)",
)
for name in sorted(DEV_CHANGING):
    check("resolve" in ancestors(name), f"{name} runs after resolve")
for name, step in steps.items():
    if "deployment" not in events(step) or name == "resolve":
        continue
    check(
        mode(step) not in ("version", "resolve"),
        f"{name} does not choose a version on the deployment path",
    )
    commands = "\n".join(step.get("commands", []) or [])
    check(
        not re.search(r">\s*\.release-tag", commands),
        f"{name} does not write .release-tag",
    )

print("==> the git tag means deployed (release-flow)")
tag = steps.get("tag", {})
check(mode(tag) in ("tag", "push-tag"), "tag runs the plugin's tag mode")
check("deploy-dev" in (tag.get("depends_on") or []), "tag waits for deploy-dev")
SMOKES = {"smoke-oidc-login-dev", "smoke-sql-console-dev", "smoke-web-live-dev"}
check(not (ancestors("tag") & SMOKES), "tag does not wait for the smokes")
check(
    not any("tag" in ancestors(name) for name in SMOKES),
    "no smoke waits for tag (they run beside it)",
)
check(
    [n for n, s in steps.items() if mode(s) in ("tag", "push-tag")] == ["tag"],
    "exactly one step tags",
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

print("==> every workflow versions with one pinned release-versions image (#476)")
plugin_images = set()
for path in (".woodpecker/web.yml", ".woodpecker/android.yml", ".woodpecker/verify-tag-deploy.yml"):
    with open(path) as handle:
        raw = handle.read()
    workflow = yaml.safe_load(raw)
    version = workflow["steps"].get("version", {})
    image = version.get("image", "")
    plugin_images.add(image)
    check(bool(PLUGIN.match(image)), f"{path} version uses the plugin pinned by digest ({image})")
    settings = version.get("settings") or {}
    check(settings.get("mode") == "version", f"{path} version runs mode version")
    check(settings.get("major_minor_source") == "cargo", f"{path} version reads Cargo.toml")
    check(
        (settings.get("github_token") or {}).get("from_secret") == "github_token",
        f"{path} version gets the github_token (it claims refs/builds/* on the private repo)",
    )
    check("release-version.sh" not in raw, f"{path} no longer calls scripts/release-version.sh")
    if path != ".woodpecker/verify-tag-deploy.yml":
        check(
            "docker push" not in raw,
            f"{path} pushes no image itself (publish does, after every test)",
        )
check(len(plugin_images) == 1, f"all three workflows pin the same plugin image ({sorted(plugin_images)})")
for name, step in steps.items():
    if mode(step):
        check(step.get("image") in plugin_images, f"{name} uses the same pinned plugin image")

if failures:
    print(f"\ndeploy pipeline validation FAILED ({len(failures)} check(s))")
    sys.exit(1)
print("\ndeploy pipeline OK")
PY
