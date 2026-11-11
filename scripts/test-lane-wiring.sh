#!/usr/bin/env bash
# Validate how the lane-skip gates are wired into the workflows (#467, #476).
#
# The lane-skip plugin owns the key and the markers and tests them in its own
# repo; this tests the YAML that decides when it is consulted, which matters
# more here. A marker written for a run that never passed makes every later
# pipeline with that key skip silently. This enforces:
#   1. the gated steps (those opening with a skip guard) are exactly e2e-web
#      and both Android instrumented lanes, and each lane step name appears in
#      lanes.yml;
#   2. a gated step's first command is the skip guard for its own lane step,
#      so a skip ends the step before anything else runs;
#   3. it depends on a lane-check step (mode check) in the same workflow whose
#      `steps` include that lane step, so the decision exists before it runs;
#   4. exactly one mark step (mode mark) records that lane step, and it depends
#      directly on the gated step and has no `when: status` override, so
#      Woodpecker runs it only after the gated step succeeded;
#   5. every lane-skip step gets github_token (markers on the private repo)
#      and, unless it is the cron's `prune` (which reads no lanes, #469), names
#      `config: lanes.yml` (the plugin's default, .woodpecker/lanes.yml,
#      is parsed by Woodpecker as a workflow and breaks every pipeline), and all
#      of them use one image pinned by digest.
# Negative cases mutate the parsed workflows to prove each rule bites.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - <<'PY'
import copy
import glob
import re
import sys

import yaml

GATED = {
    "e2e-web": "e2e-web",
    "android-instrumented-api-29": "android-api-29",
    "android-instrumented-api-36": "android-api-36",
}
IMAGE = re.compile(r"^ghcr\.io/vcheesbrough/woodpecker-plugin-lane-skip:\d+\.\d+\.\d+@sha256:[0-9a-f]{64}$")
GUARD = re.compile(r"\[ ! -e \.lane/(\S+)\.skip \] \|\| \{ cat \.lane/\1\.skip; exit 0; \}")


def lane_step(step, raw):
    """The lane step name a guard names, with $$ANDROID_API_LEVEL resolved."""
    level = str((step.get("environment") or {}).get("ANDROID_API_LEVEL", ""))
    return raw.replace("$$ANDROID_API_LEVEL", level)


def uses_image(step):
    return "woodpecker-plugin-lane-skip" in str(step.get("image", ""))


def plugin_steps(doc):
    """Steps that run the plugin itself (settings), not commands on its image."""
    return {
        name: step
        for name, step in (doc.get("steps") or {}).items()
        if uses_image(step) and step.get("settings")
    }


def check(workflows, lanes):
    problems = []
    declared = {s for lane in lanes["lanes"].values() for s in lane.get("steps", [])}
    gated = {}
    images = set()
    for path, doc in sorted(workflows.items()):
        steps = doc.get("steps") or {}
        plugins = plugin_steps(doc)
        # Every use of the image (checks' lane-files too) shares one pin, so the
        # covered-file lists the policy test reads are what the gates hash.
        images |= {s.get("image") for s in steps.values() if uses_image(s)}
        for name, step in plugins.items():
            token = (step.get("settings") or {}).get("github_token")
            if token != {"from_secret": "github_token"}:
                problems.append(f"{path} {name}: github_token is not from_secret github_token")
            # prune (#469) works on markers alone; every other mode, and a step with
            # no or a misspelt mode, must name lanes.yml.
            if (step.get("settings") or {}).get("mode") != "prune" and (
                step.get("settings") or {}
            ).get("config") != "lanes.yml":
                problems.append(f"{path} {name}: config is not lanes.yml")
        for name, step in steps.items():
            commands = [str(c) for c in step.get("commands") or []]
            reads_skip = re.compile(r"\.lane/\S+\.skip")
            if not commands or not reads_skip.search(commands[0]):
                if any(reads_skip.search(c) for c in commands):
                    problems.append(f"{path} {name}: reads a .lane/*.skip file but its first command is not the skip guard")
                continue
            where = f"{path} {name}"
            match = GUARD.fullmatch(commands[0].strip())
            if not match:
                problems.append(f"{where}: first command is not the exact skip guard")
                continue
            lane = lane_step(step, match.group(1))
            gated[name] = lane
            if lane not in declared:
                problems.append(f"{where}: guards {lane}, which no lane in lanes.yml declares")

            checks = [
                n for n in step.get("depends_on") or []
                if n in plugins
                and (plugins[n].get("settings") or {}).get("mode") == "check"
                and lane in (plugins[n]["settings"].get("steps") or [])
            ]
            if not checks:
                problems.append(f"{where}: depends on no lane-check whose steps include {lane}")

            marks = [
                (n, s) for n, s in plugins.items()
                if (s.get("settings") or {}).get("mode") == "mark"
                and (s["settings"].get("step")) == lane
            ]
            if len(marks) != 1:
                problems.append(f"{where}: {len(marks)} mark steps record {lane}, not one")
                continue
            mark_name, mark = marks[0]
            if name not in (mark.get("depends_on") or []):
                problems.append(f"{where}: {mark_name} does not depend on {name}")
            for rule in mark.get("when") or []:
                if "status" in rule:
                    problems.append(f"{where}: {mark_name} overrides `when: status`, so it could mark a failure")

        for name, step in plugins.items():
            settings = step.get("settings") or {}
            if settings.get("mode") == "mark":
                deps = step.get("depends_on") or []
                if not any(gated.get(d) == settings.get("step") for d in deps):
                    problems.append(f"{path} {name}: marks {settings.get('step')} without depending on its gated step")

    if gated != GATED:
        problems.append(f"gated steps are {gated}, expected {GATED}")
    if len(images) != 1 or not all(IMAGE.match(str(i)) for i in images):
        problems.append(f"lane-skip steps must share one digest-pinned image, got {sorted(map(str, images))}")
    return problems


workflows = {p: yaml.safe_load(open(p)) for p in glob.glob(".woodpecker/*.yml")}
lanes = yaml.safe_load(open("lanes.yml"))
failed = False


def report(ok, message):
    global failed
    print(("ok   " if ok else "FAIL ") + message)
    failed |= not ok


problems = check(workflows, lanes)
for p in problems:
    report(False, p)
report(not problems, "lane-skip gates are wired: check first, guard first, mark only after a pass")


def mutated(path, step, change):
    copied = copy.deepcopy(workflows)
    change(copied[path]["steps"][step])
    return check(copied, lanes)


def drop_guard(step):
    step["commands"] = step["commands"][1:] + ["cat .lane/x.skip"]


def guard_other_lane(step):
    step["commands"][0] = step["commands"][0].replace("e2e-web", "android-api-29")


def drop_check_dependency(step):
    step["depends_on"] = [d for d in step["depends_on"] if d != "lane-check"]


def mark_on_failure(step):
    step["when"] = [{"status": ["success", "failure"]}]


def mark_unrelated(step):
    step["depends_on"] = ["build-web"]


def drop_token(step):
    step["settings"].pop("github_token")


def default_config(step):
    step["settings"].pop("config")


def prune_step_unnamed_mode(step):
    step["settings"]["mode"] = "prnue"


def unpinned(step):
    step["image"] = "ghcr.io/vcheesbrough/woodpecker-plugin-lane-skip:latest"


cases = [
    ("skip guard removed", ".woodpecker/android.yml", "android-instrumented-api-29", drop_guard),
    ("guard naming another lane step", ".woodpecker/web.yml", "e2e-web", guard_other_lane),
    ("gated step not waiting for lane-check", ".woodpecker/android.yml", "android-instrumented-api-36", drop_check_dependency),
    ("mark that also runs on failure", ".woodpecker/android.yml", "lane-mark-android-api-36", mark_on_failure),
    ("mark depending on the wrong step", ".woodpecker/web.yml", "lane-mark-e2e-web", mark_unrelated),
    ("check without github_token", ".woodpecker/web.yml", "lane-check", drop_token),
    ("unpinned plugin image", ".woodpecker/android.yml", "lane-check", unpinned),
    ("mark falling back to the default config path", ".woodpecker/android.yml", "lane-mark-android-api-29", default_config),
    ("a misspelt prune mode escaping the config rule", ".woodpecker/prune-lane-markers.yml", "prune", prune_step_unnamed_mode),
]
for label, path, step, change in cases:
    found = mutated(path, step, change)
    report(bool(found), f"catches: {label}" + (f" ({found[0]})" if found else ""))

if failed:
    sys.exit(1)
print("\nlane wiring OK")
PY
