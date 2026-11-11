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
# Negative cases mutate the parsed workflows to prove each rule bites.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - "$@" <<'PY'
import copy
import re
import sys

import yaml

PLUGIN = re.compile(
    r"^ghcr\.io/vcheesbrough/woodpecker-plugin-release-versions:\d+\.\d+\.\d+@sha256:[0-9a-f]{64}$"
)
DEPLOY = ".woodpecker/verify-tag-deploy.yml"
BUILDS = (".woodpecker/web.yml", ".woodpecker/android.yml")
IMAGES = ["registry.desync.link/v-note", "registry.desync.link/v-note-android"]
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
SMOKES = {"smoke-oidc-login-dev", "smoke-sql-console-dev", "smoke-web-live-dev"}
# Anything touching shared infrastructure must be caught on the build path too,
# not only the steps named above: the blueprint plugin, the deploy image, or a
# secret that writes somewhere.
WRITE_SECRETS = {"authentik_api_token", "grafana_api_token", "v_note_devops_sovereign_access_url"}
# A step "writes .release-tag" when it names the file other than to read it.
READS_RELEASE_TAG = re.compile(r"\$\$?\(cat \.release-tag\)|cat \.release-tag")


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


def mode(step):
    return (step.get("settings") or {}).get("mode")


def check(docs, raws):
    """Returns (label, ok) pairs; every rule is evaluated, nothing short-circuits."""
    results = []

    def ok(condition, message):
        results.append((message, bool(condition)))

    deploy = docs[DEPLOY]
    workflow_events = set()
    for rule in deploy.get("when", []):
        events = rule.get("event", [])
        workflow_events |= set([events] if isinstance(events, str) else events)
    ok(workflow_events == {"push", "manual", "deployment"},
       "the workflow runs on push, manual and deployment only")
    steps = deploy["steps"]

    def events(step):
        rules = step.get("when")
        if not rules:
            return set(workflow_events)
        found = set()
        for rule in rules:
            value = rule.get("event", sorted(workflow_events))
            found |= set([value] if isinstance(value, str) else value)
        return found

    def targets_dev(step):
        return bool(step.get("when")) and all(
            rule.get("evaluate") == 'CI_PIPELINE_DEPLOY_TARGET == "dev"'
            for rule in step.get("when", [])
        )

    def ancestors(name, seen=None):
        seen = set() if seen is None else seen
        for dep in steps.get(name, {}).get("depends_on", []) or []:
            if dep not in seen:
                seen.add(dep)
                ancestors(dep, seen)
        return seen

    # 1. the build path
    build_path = {"version", "publish"}
    for event in ("push", "manual"):
        on_event = {n for n, s in steps.items() if event in events(s)}
        ok(on_event == build_path, f"{event} runs exactly {sorted(build_path)} (got {sorted(on_event)})")
    ok(mode(steps.get("version", {})) == "version", "version runs the plugin's version mode")
    publish = steps.get("publish", {})
    ok(mode(publish) == "publish", "publish runs the plugin's publish mode")
    ok("version" in (publish.get("depends_on") or []), "publish waits for version")
    ok((publish.get("settings") or {}).get("images") == IMAGES, "publish pushes both shipped images")
    ok({d.get("name") for d in deploy.get("depends_on", [])} == {"checks", "web", "android"},
       "the workflow waits for checks, web and android (publish follows every test)")
    for name in sorted(DEV_CHANGING):
        step = steps.get(name)
        ok(step is not None, f"{name} exists")
        if step is None:
            continue
        ok(events(step) == {"deployment"}, f"{name} runs on deployment only")
        ok(targets_dev(step), f"{name} runs only for target dev")
    for name in sorted(build_path):
        step = steps.get(name, {})
        image = step.get("image", "")
        ok("authentik-blueprint" not in image and "sovereign-config-cli" not in image,
           f"build step {name} uses neither the blueprint plugin nor the deploy image")
        ok(not (secrets(step) & WRITE_SECRETS),
           f"build step {name} holds no Authentik, Grafana or deploy credential")

    # 2. resolve chooses
    resolve = steps.get("resolve", {})
    ok(events(resolve) == {"deployment"}, "resolve runs on deployment only")
    ok(mode(resolve) == "resolve", "resolve runs the plugin's resolve mode")
    ok((resolve.get("settings") or {}).get("images") == IMAGES, "resolve checks the images publish pushes")
    ok((resolve.get("settings") or {}).get("environments") == {"dev": {}},
       "resolve allows target dev only, from any branch (prod is #388)")
    for name in sorted(DEV_CHANGING):
        ok("resolve" in ancestors(name), f"{name} runs after resolve")
    for name, step in steps.items():
        if "deployment" not in events(step) or name == "resolve":
            continue
        ok(mode(step) not in ("version", "resolve"), f"{name} does not choose a version on the deployment path")
        commands = "\n".join(str(c) for c in step.get("commands", []) or [])
        ok(".release-tag" not in READS_RELEASE_TAG.sub("", commands),
           f"{name} only reads .release-tag (never writes, copies or tees it)")

    # 3. the tag means deployed
    tag = steps.get("tag", {})
    ok(mode(tag) in ("tag", "push-tag"), "tag runs the plugin's tag mode")
    ok("deploy-dev" in (tag.get("depends_on") or []), "tag waits for deploy-dev")
    ok(not (ancestors("tag") & SMOKES), "tag does not wait for the smokes")
    ok(not any("tag" in ancestors(n) for n in SMOKES), "no smoke waits for tag (they run beside it)")
    ok([n for n, s in steps.items() if mode(s) in ("tag", "push-tag")] == ["tag"], "exactly one step tags")

    # 4. no pruned dependency
    for name, step in steps.items():
        for dep in step.get("depends_on", []) or []:
            ok(dep in steps, f"{name}: dependency {dep} exists")
            if dep in steps:
                ok(events(step) <= events(steps[dep]), f"{name}: {dep} runs on every event {name} does")

    # 5. one pinned version step everywhere; only publish pushes
    plugin_images = set()
    for path in BUILDS + (DEPLOY,):
        version = docs[path]["steps"].get("version", {})
        image = version.get("image", "")
        plugin_images.add(image)
        ok(PLUGIN.match(image), f"{path} version uses the plugin pinned by digest ({image})")
        settings = version.get("settings") or {}
        ok(settings.get("mode") == "version", f"{path} version runs mode version")
        ok(settings.get("major_minor_source") == "cargo", f"{path} version reads Cargo.toml")
        ok((settings.get("github_token") or {}).get("from_secret") == "github_token",
           f"{path} version gets the github_token (it claims refs/builds/* on the private repo)")
        ok("release-version.sh" not in raws[path], f"{path} no longer calls scripts/release-version.sh")
    for path in BUILDS:
        commands = "\n".join(
            str(c) for s in docs[path]["steps"].values() for c in s.get("commands", []) or []
        )
        ok("docker push" not in commands, f"{path} pushes no image itself (publish does, after every test)")
    ok(len(plugin_images) == 1, f"all three workflows pin the same plugin image ({sorted(plugin_images)})")
    for name, step in steps.items():
        if mode(step):
            ok(step.get("image") in plugin_images, f"{name} uses the same pinned plugin image")
    return results


raws = {p: open(p).read() for p in BUILDS + (DEPLOY,)}
docs = {p: yaml.safe_load(raw) for p, raw in raws.items()}
failures = []

for message, passed in check(docs, raws):
    print(("ok   " if passed else "FAIL ") + message)
    if not passed:
        failures.append(message)


# --- negative cases: each mutation must make at least one rule fail ---------
def step(d, path, name):
    return d[path]["steps"][name]


def deploy_on_push(d):
    step(d, DEPLOY, "deploy-dev")["when"] = [{"event": ["push", "deployment"]}]


def tag_after_smoke(d):
    step(d, DEPLOY, "tag")["depends_on"].append("smoke-web-live-dev")


def tag_on_push(d):
    step(d, DEPLOY, "tag")["when"] = [{"event": "push"}]


def blueprint_before_resolve(d):
    step(d, DEPLOY, "apply-authentik-blueprint-dev")["depends_on"] = []


def deploy_writes_release_tag(d):
    step(d, DEPLOY, "deploy-dev")["commands"].insert(0, "echo 0.0.1 | tee .release-tag")


def build_pushes_image(d):
    step(d, ".woodpecker/web.yml", "build-web")["commands"].append('docker push "$$IMAGE"')


def drifted_pin(d):
    step(d, ".woodpecker/android.yml", "version")["image"] = (
        "ghcr.io/vcheesbrough/woodpecker-plugin-release-versions:0.2.0"
    )


def publish_one_image(d):
    step(d, DEPLOY, "publish")["settings"]["images"] = IMAGES[:1]


def prod_allowed(d):
    step(d, DEPLOY, "resolve")["settings"]["environments"] = {"dev": {}, "prod": {}}


def credential_on_build_path(d):
    step(d, DEPLOY, "publish")["settings"]["extra"] = {"from_secret": "authentik_api_token"}


cases = [
    ("deploy-dev drifting onto push", deploy_on_push),
    ("tag waiting for a smoke", tag_after_smoke),
    ("tag on push", tag_on_push),
    ("blueprint applied before resolve", blueprint_before_resolve),
    ("a deploy step writing .release-tag via tee", deploy_writes_release_tag),
    ("build-web pushing its own image", build_pushes_image),
    ("a workflow on an unpinned plugin image", drifted_pin),
    ("publish dropping the android image", publish_one_image),
    ("resolve allowing an unknown target", prod_allowed),
    ("a write credential on the build path", credential_on_build_path),
]
for label, mutate in cases:
    mutated = copy.deepcopy(docs)
    mutate(mutated)
    caught = [m for m, passed in check(mutated, raws) if not passed]
    print(("ok   " if caught else "FAIL ") + f"catches: {label}" + (f" ({caught[0]})" if caught else ""))
    if not caught:
        failures.append(f"mutation not caught: {label}")

if failures:
    print(f"\ndeploy pipeline validation FAILED ({len(failures)} check(s))")
    sys.exit(1)
print("\ndeploy pipeline OK")
PY
