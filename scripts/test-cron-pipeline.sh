#!/usr/bin/env bash
# Validate the cron side of .woodpecker/ without running it (#469).
#
# v-note's first cron (prune-lane-markers) fires every workflow whose `when`
# admits the `cron` event, so this enforces both sides:
#   1. prune-lane-markers.yml runs on that cron only, and does nothing but
#      prune lane markers: one lane-skip step in mode `prune`, with a whole
#      number of days of at least 7 (an active branch's markers must survive)
#      and the github_token it deletes with;
#   2. no other workflow can run on a cron: every `when` rule either names its
#      events without `cron`, or is `evaluate: 'false'`. A rule with no
#      `event` filter matches every event, cron included.
# Negative cases mutate the parsed workflows to prove each rule bites.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - <<'PY'
import copy
import glob
import re
import sys

import yaml

PRUNE = ".woodpecker/prune-lane-markers.yml"
IMAGE = re.compile(r"^ghcr\.io/vcheesbrough/woodpecker-plugin-lane-skip:\d+\.\d+\.\d+@sha256:[0-9a-f]{64}$")


def events(rule):
    value = rule.get("event")
    if value is None:
        return None
    return set([value] if isinstance(value, str) else value)


def check(workflows):
    problems = []
    prune = workflows.get(PRUNE)
    if prune is None:
        return [f"{PRUNE} is missing"]

    if prune.get("when") != [{"event": "cron", "cron": "prune-lane-markers"}]:
        problems.append(f"{PRUNE}: when is not exactly the prune-lane-markers cron ({prune.get('when')})")
    steps = prune.get("steps") or {}
    if list(steps) != ["prune"]:
        problems.append(f"{PRUNE}: steps are {list(steps)}, expected only prune")
    step = steps.get("prune", {})
    if not IMAGE.match(str(step.get("image", ""))):
        problems.append(f"{PRUNE}: prune does not use the lane-skip plugin pinned by digest")
    if step.get("commands") or step.get("entrypoint"):
        problems.append(f"{PRUNE}: prune runs commands; it must only run the plugin")
    settings = step.get("settings") or {}
    if settings.get("mode") != "prune":
        problems.append(f"{PRUNE}: mode is {settings.get('mode')!r}, not prune")
    days = settings.get("days")
    if not (isinstance(days, int) and not isinstance(days, bool) and days >= 7):
        problems.append(f"{PRUNE}: days is {days!r}; a whole number of at least 7")
    if settings.get("dry_run") not in (None, False):
        problems.append(f"{PRUNE}: dry_run is set, so the schedule would delete nothing")
    if settings.get("github_token") != {"from_secret": "github_token"}:
        problems.append(f"{PRUNE}: github_token is not from_secret github_token")

    for path, doc in sorted(workflows.items()):
        if path == PRUNE:
            continue
        rules = doc.get("when") or [{}]
        for rule in rules:
            if rule.get("evaluate") == "false":
                continue
            found = events(rule)
            if found is None:
                problems.append(f"{path}: a when rule has no event filter, so it also runs on cron")
            elif "cron" in found:
                problems.append(f"{path}: runs on cron")
    return problems


workflows = {p: yaml.safe_load(open(p)) for p in glob.glob(".woodpecker/*.yml")}
failed = False


def report(ok, message):
    global failed
    print(("ok   " if ok else "FAIL ") + message)
    failed |= not ok


problems = check(workflows)
for p in problems:
    report(False, p)
report(not problems, "only prune-lane-markers runs on cron, and it only prunes")


def mutated(change):
    copied = copy.deepcopy(workflows)
    change(copied)
    return check(copied)


def push_on_cron(w):
    w[".woodpecker/web.yml"]["when"] = [{"event": ["push", "manual", "cron"]}]


def unfiltered_when(w):
    w[".woodpecker/checks.yml"]["when"] = [{"branch": "master"}]


def prune_on_push(w):
    w[PRUNE]["when"] = [{"event": ["cron", "push"]}]


def prune_runs_commands(w):
    w[PRUNE]["steps"]["prune"]["commands"] = ["rm -rf ."]


def extra_step(w):
    w[PRUNE]["steps"]["mark"] = {"image": w[PRUNE]["steps"]["prune"]["image"], "settings": {"mode": "mark"}}


def one_day(w):
    w[PRUNE]["steps"]["prune"]["settings"]["days"] = 1


def dry_run(w):
    w[PRUNE]["steps"]["prune"]["settings"]["dry_run"] = True


cases = [
    ("a push workflow admitting cron", push_on_cron),
    ("a workflow with no event filter", unfiltered_when),
    ("prune also running on push", prune_on_push),
    ("prune running shell commands", prune_runs_commands),
    ("a second step in the prune workflow", extra_step),
    ("a 1-day retention", one_day),
    ("a dry-run schedule", dry_run),
]
for label, change in cases:
    found = mutated(change)
    report(bool(found), f"catches: {label}" + (f" ({found[0]})" if found else ""))

if failed:
    sys.exit(1)
print("\ncron pipeline OK")
PY
